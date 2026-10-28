//! The connection to the host, on its own thread with its own tokio runtime.
//! The rest of the client only sees std channels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use aetherdesk_proto::quic::{self, HostTrust};
use aetherdesk_proto::trust::{Identity, Role, start_pake};
use aetherdesk_proto::{
    CLOSE_NO_AGENT, CLOSE_NOT_PAIRED, CLOSE_PAIRING_CLOSED, CLOSE_SHUTDOWN, CLOSE_VERSION,
    CLOSE_WRONG_CODE, ClientControl, DEFAULT_PORT, Hello, HostControl, InputEvent, Intent,
    PROTOCOL_VERSION, PairMsg, SessionKind, VideoFrame, Welcome,
};
use anyhow::{Context, anyhow, bail};
use tokio::sync::mpsc::UnboundedSender;

use crate::latency::{ClockSync, Timing, now_us};
use crate::trust::Host;

/// How often we probe the host's clock.
const PING_EVERY: Duration = Duration::from_secs(1);

pub struct Session {
    pub welcome: Welcome,
    /// Video frames in arrival order, with their timestamps so far. Closes
    /// when the connection ends.
    pub frames: mpsc::Receiver<(VideoFrame, Timing)>,
    /// Maps host timestamps onto our clock.
    pub sync: Arc<ClockSync>,
    /// Send `()` to ask the host for a keyframe.
    pub keyframe_requests: UnboundedSender<()>,
    /// Keyboard and mouse events for the host.
    pub input: UnboundedSender<InputEvent>,
    /// The Mac shows its login window: view-only, macOS ignores our input.
    pub login_window: Arc<AtomicBool>,
    /// Why the connection ended, once it has. Set before `frames` closes, so
    /// whoever sees the end can say why.
    pub ended: Arc<Mutex<Option<String>>>,
}

/// Connects to a paired host and does the handshake; returns once the host
/// has said welcome.
pub fn connect(host: Host, id: Identity) -> anyhow::Result<Session> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new().name("net".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!("tokio: {e}")));
                return;
            }
        };
        runtime.block_on(async move {
            if let Err(e) = run(&host, &id, &ready_tx).await {
                // Before the handshake this reaches the caller. After it,
                // the reason is in `Session::ended`, which the main thread
                // prints: printing here races with the process exiting.
                let _ = ready_tx.send(Err(anyhow!("{e:#}")));
            }
        });
    })?;
    ready_rx.recv().context("network thread died")?
}

async fn resolve(host: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(addr) = host.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("resolving {with_port}"))?
        .next()
        .with_context(|| format!("{with_port} has no address"))
}

/// Opens a QUIC connection to `address` that shows our certificate and
/// accepts the host's per `trust`.
async fn dial(
    address: &str,
    id: &Identity,
    trust: HostTrust,
) -> anyhow::Result<(quinn::Endpoint, quinn::Connection)> {
    let addr = resolve(address).await?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let endpoint = quic::client_endpoint(bind, id, trust).map_err(|e| anyhow!("{e}"))?;
    let conn = match endpoint.connect(addr, quic::SERVER_NAME)?.await {
        Ok(c) => c,
        Err(e) if format!("{e:?}").contains("NotPaired") => bail!(
            "{address} answered, but with a different certificate than the Mac this client \
             paired with. If you reinstalled or reset the Mac, pair again \
             (`aetherdesk unpair` it, then `aetherdesk pair {address}`); otherwise \
             something else is answering at that address"
        ),
        Err(e) => return Err(e).with_context(|| format!("connecting to {address} ({addr})")),
    };
    Ok((endpoint, conn))
}

/// Why the host closed the connection during the handshake, in words.
fn refusal(conn: &quinn::Connection, address: &str) -> anyhow::Error {
    let Some(reason) = conn.close_reason() else {
        return anyhow!("the host closed the connection during the handshake");
    };
    let code = match &reason {
        quinn::ConnectionError::ApplicationClosed(c) => {
            u32::try_from(c.error_code.into_inner()).ok()
        }
        _ => None,
    };
    match code {
        Some(CLOSE_NOT_PAIRED) => anyhow!(
            "the Mac doesn't know this client (never paired, or unpaired there). \
             Open pairing on the Mac (scripts/mac.zsh pair) and run `aetherdesk pair {address}`"
        ),
        Some(CLOSE_VERSION) => anyhow!(
            "the Mac runs a different protocol version; update both sides to the same release"
        ),
        Some(CLOSE_NO_AGENT) => anyhow!(
            "the Mac has no agent for the session on screen; is the host installed and running? \
             (scripts/mac.zsh status)"
        ),
        Some(CLOSE_PAIRING_CLOSED) => anyhow!(
            "pairing isn't open on the Mac. Run `scripts/mac.zsh pair` there first, then try \
             again within 2 minutes"
        ),
        Some(CLOSE_WRONG_CODE) => anyhow!(
            "wrong code. After 3 wrong codes the Mac closes pairing; then start it again there"
        ),
        Some(CLOSE_SHUTDOWN) => anyhow!("the Mac host is shutting down"),
        _ => anyhow!("host refused: {reason}"),
    }
}

/// Pairs with the host at `address` using the code it shows. Returns the
/// host as it should be stored.
pub fn pair(address: &str, code: &str, our_name: &str, id: &Identity) -> anyhow::Result<Host> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let (endpoint, conn) = dial(address, id, HostTrust::Pairing).await?;
        let host_fp = quic::peer_fingerprint(&conn).context("the host showed no certificate")?;
        let ours = id.fingerprint();
        let (mut send, mut recv) = conn.open_bi().await?;
        let result = async {
            quic::send(
                &mut send,
                &Hello {
                    version: PROTOCOL_VERSION,
                    intent: Intent::Pair {
                        name: our_name.to_string(),
                    },
                },
            )
            .await?;
            let (pake, msg) = start_pake(code);
            quic::send(&mut send, &PairMsg::Spake(msg)).await?;
            let Ok(Some(PairMsg::Spake(theirs))) = quic::recv(&mut recv).await else {
                return Err(refusal(&conn, address));
            };
            let key = pake.finish(&theirs).map_err(|e| anyhow!(e))?;
            quic::send(
                &mut send,
                &PairMsg::Confirm(key.confirm(Role::Client, &ours, &host_fp)),
            )
            .await?;
            let Ok(Some(PairMsg::Paired { name, confirm })) = quic::recv(&mut recv).await else {
                return Err(refusal(&conn, address));
            };
            if !key.verify(Role::Host, &ours, &host_fp, &confirm) {
                bail!("the host's proof doesn't match; not pairing with it");
            }
            Ok(Host {
                fingerprint: host_fp,
                address: address.to_string(),
                name,
            })
        }
        .await;
        conn.close(0u32.into(), b"bye");
        endpoint.wait_idle().await;
        result
    })
}

async fn run(
    host: &Host,
    id: &Identity,
    ready: &mpsc::Sender<anyhow::Result<Session>>,
) -> anyhow::Result<()> {
    let (endpoint, conn) = dial(&host.address, id, HostTrust::Pinned(host.fingerprint)).await?;

    let (mut ctl_send, mut ctl_recv) = conn.open_bi().await?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        intent: Intent::Connect,
    };
    quic::send(&mut ctl_send, &hello).await?;
    let welcome: Welcome = match quic::recv(&mut ctl_recv).await {
        Ok(Some(w)) => w,
        _ => return Err(refusal(&conn, &host.address)),
    };
    if welcome.version != PROTOCOL_VERSION {
        bail!(
            "host speaks protocol v{}, we speak v{PROTOCOL_VERSION}",
            welcome.version
        );
    }
    let mut video = conn.accept_uni().await.context("video stream")?;

    let (frames_tx, frames) = mpsc::channel();
    let (keyframe_requests, mut keyframe_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut input_stream = conn.open_uni().await.context("input stream")?;
    let (input, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync = Arc::new(ClockSync::default());
    let login_window = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(Mutex::new(None));
    ready
        .send(Ok(Session {
            welcome,
            frames,
            sync: sync.clone(),
            keyframe_requests,
            input,
            login_window: login_window.clone(),
            ended: ended.clone(),
        }))
        .map_err(|_| anyhow!("client went away"))?;

    // Control out: keyframe requests and clock pings.
    let requests = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        loop {
            let msg = tokio::select! {
                req = keyframe_rx.recv() => match req {
                    Some(()) => ClientControl::RequestKeyframe,
                    None => break,
                },
                _ = ping.tick() => ClientControl::Ping { client_us: now_us() as u64 },
            };
            if quic::send(&mut ctl_send, &msg).await.is_err() {
                break;
            }
        }
    });
    // Control in: pongs and session changes.
    let pong_sync = sync.clone();
    let pongs = tokio::spawn(async move {
        while let Ok(Some(msg)) = quic::recv::<HostControl>(&mut ctl_recv).await {
            match msg {
                HostControl::Pong { client_us, host_us } => {
                    pong_sync.pong(client_us as i64, host_us, now_us());
                }
                HostControl::Session(kind) => {
                    let at_login = kind == SessionKind::LoginWindow;
                    if at_login {
                        println!(
                            "the Mac shows its login window: view-only, macOS ignores remote \
                             input there. Log in at the Mac or with Screen Sharing \
                             (vnc://<mac>); this connection follows to the desktop."
                        );
                    } else if login_window.load(Ordering::Relaxed) {
                        println!("logged in: input goes to the Mac again");
                    }
                    login_window.store(at_login, Ordering::Relaxed);
                }
            }
        }
    });
    let inputs = tokio::spawn(async move {
        while let Some(event) = input_rx.recv().await {
            if quic::send(&mut input_stream, &event).await.is_err() {
                break;
            }
        }
    });
    let result = loop {
        match quic::recv::<VideoFrame>(&mut video).await {
            Ok(Some(frame)) => {
                let timing = Timing::arrived(&frame.timing, &sync, now_us());
                if frames_tx.send((frame, timing)).is_err() {
                    break Ok(()); // window closed
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => {
                break match conn.close_reason() {
                    Some(reason) => Err(anyhow!("connection closed: {reason}")),
                    None => Err(e.into()),
                };
            }
        }
    };
    // The video stream can end just before the close reason arrives; give
    // it a moment, so a host shutting down is reported as such.
    if result.is_ok() {
        let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
    }
    let result = match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(c))
            if c.error_code == CLOSE_SHUTDOWN.into() =>
        {
            Err(anyhow!(
                "the Mac host is shutting down (restart, shutdown or uninstall)"
            ))
        }
        _ => result,
    };
    if let Err(e) = &result {
        *ended.lock().unwrap() = Some(format!("{e:#}"));
    }
    requests.abort();
    pongs.abort();
    inputs.abort();
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    result
}
