//! The connection to the host, on its own thread with its own tokio runtime.
//! The rest of the client only sees std channels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use aetherdesk_proto::quic::{self, HostTrust};
use aetherdesk_proto::trust::{Identity, Role, start_pake};
use aetherdesk_proto::{
    CLOSE_CODEC, CLOSE_NO_AGENT, CLOSE_NOT_PAIRED, CLOSE_PAIRING_CLOSED, CLOSE_SHUTDOWN,
    CLOSE_VERSION, CLOSE_WRONG_CODE, ClientControl, DEFAULT_PORT, Hello, HostControl, InputEvent,
    Intent, PROTOCOL_VERSION, PairMsg, SessionKind, VideoFrame, Welcome,
};
use anyhow::{Context, anyhow, bail};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::discover::Finder;
use crate::latency::{ClockSync, Timing, now_us};
use crate::trust::Host;

/// How often we probe the host's clock. Pongs also show the link is alive
/// when the Mac's screen is still and sends no frames.
const PING_EVERY: Duration = Duration::from_secs(1);

/// How long we keep trying after losing the connection, before asking.
const RETRY_FOR: Duration = Duration::from_secs(60);

/// Where the link stands, for the window to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Connected,
    /// Lost; trying again on our own. `attempt` counts from 1.
    Reconnecting {
        attempt: u32,
        reason: String,
    },
    /// Stopped trying after [`RETRY_FOR`]; a retry request starts again.
    GaveUp {
        reason: String,
    },
}

/// A connection to a paired host that comes back by itself after a drop
/// (host restart, network loss). Its channels outlive single connections.
pub struct Link {
    /// From the first connection; the picture size can change later, the
    /// decoder follows the stream.
    pub welcome: Welcome,
    /// Video frames in arrival order, with their timestamps so far. Closes
    /// when the link gives up for good (see `ended`).
    pub frames: mpsc::Receiver<(VideoFrame, Timing)>,
    /// Maps host timestamps onto our clock.
    pub sync: Arc<ClockSync>,
    /// Send `()` to ask the host for a keyframe.
    pub keyframe_requests: UnboundedSender<()>,
    /// Keyboard and mouse events for the host. Dropped while disconnected.
    pub input: UnboundedSender<InputEvent>,
    /// The Mac shows its login window: view-only, macOS ignores our input.
    pub login_window: Arc<AtomicBool>,
    pub state: Arc<Mutex<State>>,
    /// When we last heard from the host (frames or pongs), on our clock.
    pub last_heard: Arc<AtomicI64>,
    /// Send `()` to try again now, e.g. after [`State::GaveUp`].
    pub retry: UnboundedSender<()>,
    /// Why the link ended for good, once it has. Set before `frames` closes,
    /// so whoever sees the end can say why. Empty when the window closed.
    pub ended: Arc<Mutex<Option<String>>>,
    /// Ends the link with a goodbye to the host.
    pub closer: Closer,
    /// Clipboard sync, when asked for: text copied here goes out on the
    /// sender, text the Mac copied comes in on the receiver.
    pub clipboard: Option<(UnboundedSender<String>, mpsc::Receiver<String>)>,
    /// The Mac's pointer: [`HostControl::CursorImage`] and friends.
    pub pointer: mpsc::Receiver<HostControl>,
}

/// Ends a [`Link`] on purpose. The host tells "the client left" (lock the
/// Mac now) from "the link died" (wait a while, it may come back) by
/// whether the connection was closed or just went quiet, so the client has
/// to say goodbye before the process exits.
pub struct Closer {
    quit: UnboundedSender<()>,
    /// Disconnects when the network thread has finished.
    done: mpsc::Receiver<()>,
}

impl Closer {
    /// Closes the connection and waits (briefly) until the host was told.
    pub fn close(self) {
        let _ = self.quit.send(());
        let _ = self.done.recv_timeout(Duration::from_secs(2));
    }
}

/// Why a connection attempt or a session ended, and whether trying again
/// can help.
#[derive(Debug)]
struct Failure {
    reason: String,
    retry: bool,
}

impl Failure {
    fn transient(reason: impl std::fmt::Display) -> Self {
        Self {
            reason: reason.to_string(),
            retry: true,
        }
    }

    fn permanent(reason: impl std::fmt::Display) -> Self {
        Self {
            reason: reason.to_string(),
            retry: false,
        }
    }
}

/// What the link thread shares with the rest of the client.
struct Shared {
    frames: mpsc::Sender<(VideoFrame, Timing)>,
    sync: Arc<ClockSync>,
    login_window: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    last_heard: Arc<AtomicI64>,
    ended: Arc<Mutex<Option<String>>>,
    /// Text the Mac copied, for the window to put on our clipboard. `None`
    /// without clipboard sync.
    clipboard: Option<mpsc::Sender<String>>,
    /// The Mac's pointer: shapes and moves, for the window.
    pointer: mpsc::Sender<HostControl>,
}

impl Shared {
    fn set(&self, state: State) {
        *self.state.lock().unwrap() = state;
    }
}

/// The other ends of [`Link`]'s senders.
struct Requests {
    keyframes: UnboundedReceiver<()>,
    input: UnboundedReceiver<InputEvent>,
    retry: UnboundedReceiver<()>,
    /// `()` or closed: the window is gone, say goodbye and stop.
    quit: UnboundedReceiver<()>,
    /// Text copied here, for the Mac. Closed without clipboard sync.
    clipboard: UnboundedReceiver<String>,
}

/// Connects to a paired host; returns once it has said welcome. A first
/// connection that fails is an error; after that the link reconnects on its
/// own.
pub fn connect(host: Host, id: Identity, clipboard: bool) -> anyhow::Result<Link> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (quit, quit_rx) = unbounded_channel();
    let (done_tx, done) = mpsc::channel::<()>();
    let closer = Closer { quit, done };
    thread::Builder::new().name("net".into()).spawn(move || {
        let _done = done_tx;
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
        runtime.block_on(link(host, id, clipboard, ready_tx, closer, quit_rx));
    })?;
    ready_rx.recv().context("network thread died")?
}

async fn link(
    mut host: Host,
    id: Identity,
    clipboard: bool,
    ready: mpsc::Sender<anyhow::Result<Link>>,
    closer: Closer,
    quit: UnboundedReceiver<()>,
) {
    // Listens for the Mac on the LAN, in case its address changed.
    let finder = match Finder::start() {
        Ok(f) => Some(f),
        Err(e) => {
            warn!("not looking for the Mac on the LAN: {e}");
            None
        }
    };
    let finder = finder.as_ref();
    let mut live = match open_located(&mut host, &id, clipboard, finder, LOOKUP).await {
        Ok(l) => l,
        Err(f) => {
            let _ = ready.send(Err(anyhow!(f.reason)));
            return;
        }
    };
    let (frames_tx, frames) = mpsc::channel();
    let (keyframe_requests, keyframes) = unbounded_channel();
    let (input, input_rx) = unbounded_channel();
    let (retry, retry_rx) = unbounded_channel();
    let (clipboard_out, clipboard_out_rx) = unbounded_channel();
    let (clipboard_in_tx, clipboard_in) = mpsc::channel();
    let (pointer_tx, pointer) = mpsc::channel();
    let shared = Shared {
        frames: frames_tx,
        sync: Arc::new(ClockSync::default()),
        login_window: Arc::new(AtomicBool::new(false)),
        state: Arc::new(Mutex::new(State::Connected)),
        last_heard: Arc::new(AtomicI64::new(now_us())),
        ended: Arc::new(Mutex::new(None)),
        clipboard: clipboard.then_some(clipboard_in_tx),
        pointer: pointer_tx,
    };
    let mut requests = Requests {
        keyframes,
        input: input_rx,
        retry: retry_rx,
        quit,
        clipboard: clipboard_out_rx,
    };
    let sent = ready.send(Ok(Link {
        welcome: live.welcome.clone(),
        frames,
        sync: shared.sync.clone(),
        keyframe_requests,
        input,
        login_window: shared.login_window.clone(),
        state: shared.state.clone(),
        last_heard: shared.last_heard.clone(),
        retry,
        ended: shared.ended.clone(),
        closer,
        clipboard: clipboard.then_some((clipboard_out, clipboard_in)),
        pointer,
    }));
    if sent.is_err() {
        return;
    }
    loop {
        shared.set(State::Connected);
        let end = live.run(&shared, &mut requests).await;
        if !end.retry {
            *shared.ended.lock().unwrap() = Some(end.reason);
            return; // dropping `shared` closes the frames
        }
        say!("connection lost: {}; reconnecting…", end.reason);
        match reconnect(
            &mut host,
            &id,
            clipboard,
            finder,
            &shared,
            &mut requests,
            end.reason,
        )
        .await
        {
            Some(l) => {
                say!("reconnected");
                live = l;
            }
            None => return,
        }
    }
}

/// How long a connection attempt listens for the Mac on the LAN, alongside
/// trying the remembered address. It costs nothing when the remembered
/// address works (Tailscale, VPN, or simply unchanged). As long as an
/// attempt at an address that went stale takes to time out (the 5 s idle
/// timeout), so a slow mDNS answer still beats it.
const LOOKUP: Duration = Duration::from_secs(5);

/// Opens a session with the paired host, trying two ways at once: the
/// remembered address, and the address it announces on the LAN if that's
/// different. The first to get in wins. A new address is kept only once the
/// pinned certificate checked out there, so a false announcement costs one
/// failed attempt and nothing else; the remembered address decides the
/// error when neither works.
async fn open_located(
    host: &mut Host,
    id: &Identity,
    clipboard: bool,
    finder: Option<&Finder>,
    wait: Duration,
) -> Result<Live, Failure> {
    let remembered = host.clone();
    let stored = open(&remembered, id, clipboard);
    let located = async {
        let found = finder?.wait_for(&remembered.fingerprint, wait).await?;
        if same_address(&remembered.address, &found.address) {
            return None;
        }
        let moved = Host {
            address: found.address.to_string(),
            ..remembered.clone()
        };
        let result = open(&moved, id, clipboard).await;
        if let Err(f) = &result {
            warn!("no luck at {}: {}", moved.address, f.reason);
        }
        Some((moved, result))
    };
    tokio::pin!(stored, located);
    let mut stored_failed = None;
    let mut located_done = false;
    loop {
        tokio::select! {
            r = &mut stored, if stored_failed.is_none() => match r {
                Ok(live) => return Ok(live),
                Err(f) if located_done => return Err(f),
                Err(f) => stored_failed = Some(f),
            },
            r = &mut located, if !located_done => {
                located_done = true;
                if let Some((moved, Ok(live))) = r {
                    say!(
                        "found {} on the LAN at {} (was {})",
                        host.name, moved.address, host.address
                    );
                    if let Err(e) = crate::trust::Store::user()
                        .and_then(|s| s.set_address(&host.fingerprint, &moved.address))
                    {
                        warn!("couldn't remember the new address: {e}");
                    }
                    *host = moved;
                    return Ok(live);
                }
                if let Some(f) = stored_failed.take() {
                    return Err(f);
                }
            }
        }
    }
}

/// Whether a remembered address (maybe a hostname, maybe without port) is
/// the one announced.
fn same_address(remembered: &str, announced: &SocketAddr) -> bool {
    match remembered.parse::<SocketAddr>() {
        Ok(a) => a == *announced,
        Err(_) => match remembered.parse::<std::net::IpAddr>() {
            Ok(ip) => ip == announced.ip() && announced.port() == DEFAULT_PORT,
            Err(_) => false,
        },
    }
}

/// Backs off 0.5, 1, 2 s, then every 5 s, with some jitter so a restarted
/// host isn't hit by every client at once.
fn backoff(attempt: u32) -> Duration {
    let base = match attempt {
        1 => 500,
        2 => 1000,
        3 => 2000,
        _ => 5000,
    };
    let jitter = (now_us().rem_euclid(1000) as u64 * base / 5000) as i64 - base as i64 / 10;
    Duration::from_millis((base as i64 + jitter) as u64)
}

/// Tries to get a new connection after a drop. `None` when trying can't help
/// any more (a refusal like "not paired") or the window is gone.
async fn reconnect(
    host: &mut Host,
    id: &Identity,
    clipboard: bool,
    finder: Option<&Finder>,
    shared: &Shared,
    requests: &mut Requests,
    mut reason: String,
) -> Option<Live> {
    loop {
        let start = Instant::now();
        let mut attempt = 0;
        while start.elapsed() < RETRY_FOR {
            attempt += 1;
            shared.set(State::Reconnecting {
                attempt,
                reason: reason.clone(),
            });
            tokio::select! {
                _ = tokio::time::sleep(backoff(attempt)) => {}
                r = requests.retry.recv() => r?,
                _ = requests.quit.recv() => return None,
            }
            match open_located(host, id, clipboard, finder, LOOKUP).await {
                Ok(live) => return Some(live),
                Err(f) if !f.retry => {
                    *shared.ended.lock().unwrap() = Some(f.reason);
                    return None;
                }
                Err(f) => reason = f.reason,
            }
        }
        say!(
            "still no connection after {} s: {reason}",
            RETRY_FOR.as_secs()
        );
        shared.set(State::GaveUp {
            reason: reason.clone(),
        });
        tokio::select! {
            r = requests.retry.recv() => r?,
            _ = requests.quit.recv() => return None,
        }
    }
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
) -> Result<(quinn::Endpoint, quinn::Connection), Failure> {
    let addr = resolve(address)
        .await
        .map_err(|e| Failure::transient(format!("{e:#}")))?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .expect("valid");
    let endpoint =
        quic::client_endpoint(bind, id, trust).map_err(|e| Failure::permanent(e.to_string()))?;
    let connecting = endpoint
        .connect(addr, quic::SERVER_NAME)
        .map_err(|e| Failure::permanent(e.to_string()))?;
    let conn = match connecting.await {
        Ok(c) => c,
        Err(e) if format!("{e:?}").contains("NotPaired") => {
            return Err(Failure::permanent(format!(
                "{address} answered, but with a different certificate than the Mac this client \
                 paired with. If you reinstalled or reset the Mac, pair again \
                 (`aetherdesk unpair` it, then `aetherdesk pair {address}`); otherwise \
                 something else is answering at that address"
            )));
        }
        Err(e) => {
            return Err(Failure::transient(format!(
                "connecting to {address} ({addr}): {e}"
            )));
        }
    };
    Ok((endpoint, conn))
}

/// The application close code, if the host closed the connection with one.
fn close_code(conn: &quinn::Connection) -> Option<u32> {
    match conn.close_reason()? {
        quinn::ConnectionError::ApplicationClosed(c) => {
            u32::try_from(c.error_code.into_inner()).ok()
        }
        _ => None,
    }
}

/// Why the host closed the connection during the handshake, in words.
fn refusal(conn: &quinn::Connection, address: &str) -> Failure {
    let Some(reason) = conn.close_reason() else {
        return Failure::transient("the host closed the connection during the handshake");
    };
    match close_code(conn) {
        Some(CLOSE_NOT_PAIRED) => Failure::permanent(format!(
            "the Mac doesn't know this client (never paired, or unpaired there). \
             Open pairing on the Mac (scripts/mac.zsh pair) and run `aetherdesk pair {address}`"
        )),
        Some(CLOSE_VERSION) => Failure::permanent(
            "the Mac runs a different protocol version; update both sides to the same release",
        ),
        Some(CLOSE_NO_AGENT) => Failure::transient(
            "the Mac has no agent for the session on screen; is the host installed and running? \
             (scripts/mac.zsh status)",
        ),
        Some(CLOSE_PAIRING_CLOSED) => Failure::permanent(
            "pairing isn't open on the Mac. Run `scripts/mac.zsh pair` there first, then try \
             again within 2 minutes",
        ),
        Some(CLOSE_WRONG_CODE) => Failure::permanent(
            "wrong code. After 3 wrong codes the Mac closes pairing; then start it again there",
        ),
        Some(CLOSE_SHUTDOWN) => Failure::transient("the Mac host is shutting down"),
        Some(CLOSE_CODEC) => Failure::permanent(format!(
            "the Mac sends no video format this client decodes (we decode {}); update one of them",
            crate::decoder::codecs()
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => Failure::transient(format!("host refused: {reason}")),
    }
}

/// Pairs with the host at `address` using the code it shows. Returns the
/// host as it should be stored.
pub fn pair(address: &str, code: &str, our_name: &str, id: &Identity) -> anyhow::Result<Host> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let (endpoint, conn) = dial(address, id, HostTrust::Pairing)
            .await
            .map_err(|f| anyhow!(f.reason))?;
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
                return Err(anyhow!(refusal(&conn, address).reason));
            };
            let key = pake.finish(&theirs).map_err(|e| anyhow!(e))?;
            quic::send(
                &mut send,
                &PairMsg::Confirm(key.confirm(Role::Client, &ours, &host_fp)),
            )
            .await?;
            let Ok(Some(PairMsg::Paired { name, confirm })) = quic::recv(&mut recv).await else {
                return Err(anyhow!(refusal(&conn, address).reason));
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

/// One connection, after the host said welcome.
struct Live {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    welcome: Welcome,
    ctl_send: quinn::SendStream,
    ctl_recv: quinn::RecvStream,
    video: quinn::RecvStream,
    input: quinn::SendStream,
}

/// Connects and does the handshake.
async fn open(host: &Host, id: &Identity, clipboard: bool) -> Result<Live, Failure> {
    let (endpoint, conn) = dial(&host.address, id, HostTrust::Pinned(host.fingerprint)).await?;
    let lost = |e: &dyn std::fmt::Display| Failure::transient(format!("handshake: {e}"));
    let (mut ctl_send, mut ctl_recv) = conn.open_bi().await.map_err(|e| lost(&e))?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        intent: Intent::Connect {
            clipboard,
            codecs: crate::decoder::codecs().to_vec(),
        },
    };
    quic::send(&mut ctl_send, &hello)
        .await
        .map_err(|e| lost(&e))?;
    let welcome: Welcome = match quic::recv(&mut ctl_recv).await {
        Ok(Some(w)) => w,
        _ => return Err(refusal(&conn, &host.address)),
    };
    if welcome.version != PROTOCOL_VERSION {
        return Err(Failure::permanent(format!(
            "host speaks protocol v{}, we speak v{PROTOCOL_VERSION}",
            welcome.version
        )));
    }
    if !crate::decoder::codecs().contains(&welcome.codec) {
        return Err(Failure::permanent(format!(
            "the Mac sends {}, which this client didn't offer",
            welcome.codec
        )));
    }
    let video = conn.accept_uni().await.map_err(|e| lost(&e))?;
    let input = conn.open_uni().await.map_err(|e| lost(&e))?;
    Ok(Live {
        endpoint,
        conn,
        welcome,
        ctl_send,
        ctl_recv,
        video,
        input,
    })
}

impl Live {
    /// Runs the session until the connection ends, and says why.
    async fn run(self, shared: &Shared, requests: &mut Requests) -> Failure {
        let Live {
            endpoint,
            conn,
            mut ctl_send,
            mut ctl_recv,
            mut video,
            input: mut input_stream,
            ..
        } = self;
        // What piled up while disconnected is stale: old keys, old clicks.
        while requests.input.try_recv().is_ok() {}
        while requests.keyframes.try_recv().is_ok() {}
        shared.sync.reset();
        shared.last_heard.store(now_us(), Ordering::Relaxed);

        // Control in: pongs and session changes.
        let (sync, login_window, heard, copied, pointer) = (
            shared.sync.clone(),
            shared.login_window.clone(),
            shared.last_heard.clone(),
            shared.clipboard.clone(),
            shared.pointer.clone(),
        );
        let pongs = tokio::spawn(async move {
            while let Ok(Some(msg)) = quic::recv::<HostControl>(&mut ctl_recv).await {
                heard.store(now_us(), Ordering::Relaxed);
                match msg {
                    HostControl::Pong { client_us, host_us } => {
                        sync.pong(client_us as i64, host_us, now_us());
                    }
                    HostControl::Session(kind) => {
                        let at_login = kind == SessionKind::LoginWindow;
                        if at_login {
                            say!(
                                "the Mac shows its login window: view-only, macOS ignores remote \
                                 input there. Log in at the Mac or with Screen Sharing \
                                 (vnc://<mac>); this connection follows to the desktop."
                            );
                        } else if login_window.load(Ordering::Relaxed) {
                            say!("logged in: input goes to the Mac again");
                        }
                        login_window.store(at_login, Ordering::Relaxed);
                    }
                    HostControl::Clipboard(text) => {
                        if let Some(c) = &copied {
                            let _ = c.send(text);
                        }
                    }
                    msg @ (HostControl::CursorImage(_)
                    | HostControl::CursorShow(_)
                    | HostControl::CursorMoved { .. }
                    | HostControl::CursorInVideo) => {
                        let _ = pointer.send(msg);
                    }
                }
            }
        });
        // Video in, on its own task: a read can't be cut off halfway.
        let (frames, sync, heard) = (
            shared.frames.clone(),
            shared.sync.clone(),
            shared.last_heard.clone(),
        );
        let mut frames_in = tokio::spawn(async move {
            loop {
                match quic::recv::<VideoFrame>(&mut video).await {
                    Ok(Some(frame)) => {
                        heard.store(now_us(), Ordering::Relaxed);
                        let timing = Timing::arrived(&frame.timing, &sync, now_us());
                        if frames.send((frame, timing)).is_err() {
                            return VideoEnd::WindowClosed;
                        }
                    }
                    Ok(None) => return VideoEnd::Finished,
                    Err(e) => return VideoEnd::Error(e.to_string()),
                }
            }
        });

        // Out: keyframe requests and pings on control, input on its stream.
        let mut ping = tokio::time::interval(PING_EVERY);
        let video_end = loop {
            tokio::select! {
                end = &mut frames_in => break end.unwrap_or(VideoEnd::Finished),
                _ = requests.quit.recv() => {
                    frames_in.abort();
                    break VideoEnd::WindowClosed;
                }
                Some(()) = requests.keyframes.recv() => {
                    let _ = quic::send(&mut ctl_send, &ClientControl::RequestKeyframe).await;
                }
                Some(event) = requests.input.recv() => {
                    let _ = quic::send(&mut input_stream, &event).await;
                }
                Some(text) = requests.clipboard.recv() => {
                    let _ = quic::send(&mut ctl_send, &ClientControl::Clipboard(text)).await;
                }
                _ = ping.tick() => {
                    let msg = ClientControl::Ping { client_us: now_us() as u64 };
                    let _ = quic::send(&mut ctl_send, &msg).await;
                }
            }
        };
        if matches!(video_end, VideoEnd::Finished) {
            // The stream can end just before the close reason arrives.
            let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
        }
        let failure = match (video_end, conn.close_reason()) {
            (VideoEnd::WindowClosed, _) => Failure::permanent(""),
            (_, Some(quinn::ConnectionError::TimedOut)) => {
                Failure::transient("no answer from the Mac (network or host down)")
            }
            (_, Some(reason)) => match close_code(&conn) {
                Some(CLOSE_SHUTDOWN) => Failure::transient(
                    "the Mac host is shutting down (restart, shutdown or uninstall)",
                ),
                Some(CLOSE_NO_AGENT) => Failure::transient("the Mac has no agent running"),
                Some(0) => Failure::permanent("the host ended the session"),
                _ => Failure::transient(format!("connection closed: {reason}")),
            },
            (VideoEnd::Finished, None) => Failure::permanent("the host ended the session"),
            (VideoEnd::Error(e), None) => Failure::transient(e),
        };
        pongs.abort();
        conn.close(0u32.into(), b"bye");
        let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
        failure
    }
}

/// How the video stream ended.
enum VideoEnd {
    /// The host finished it.
    Finished,
    /// The window is gone; nobody wants frames.
    WindowClosed,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_addresses_compare_with_announced_ones() {
        let announced: SocketAddr = "192.0.2.10:4500".parse().unwrap();
        assert!(same_address("192.0.2.10:4500", &announced));
        assert!(same_address("192.0.2.10", &announced));
        assert!(!same_address("192.0.2.11", &announced));
        assert!(!same_address("192.0.2.10:4501", &announced));
        // A hostname may well point there, but we can't tell: try both.
        assert!(!same_address("office-mac.local", &announced));
        let other_port: SocketAddr = "192.0.2.10:4501".parse().unwrap();
        assert!(!same_address("192.0.2.10", &other_port));
    }
}
