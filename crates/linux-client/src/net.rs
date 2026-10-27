//! The connection to the host, on its own thread with its own tokio runtime.
//! The rest of the client only sees std channels.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread;

use aetherdesk_proto::quic;
use aetherdesk_proto::{ClientControl, DEFAULT_PORT, Hello, PROTOCOL_VERSION, VideoFrame, Welcome};
use anyhow::{Context, anyhow, bail};
use tokio::sync::mpsc::UnboundedSender;

pub struct Session {
    pub welcome: Welcome,
    /// Video frames in arrival order. Closes when the connection ends.
    pub frames: mpsc::Receiver<VideoFrame>,
    /// Send `()` to ask the host for a keyframe.
    pub keyframe_requests: UnboundedSender<()>,
}

/// Connects and does the handshake; returns once the host has said welcome.
pub fn connect(host: &str, token: &str) -> anyhow::Result<Session> {
    let (host, token) = (host.to_string(), token.to_string());
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
            if let Err(e) = run(&host, &token, &ready_tx).await {
                // Before the handshake this reaches the caller; after, stderr.
                if ready_tx.send(Err(anyhow!("{e:#}"))).is_err() {
                    eprintln!("connection: {e:#}");
                }
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

async fn run(
    host: &str,
    token: &str,
    ready: &mpsc::Sender<anyhow::Result<Session>>,
) -> anyhow::Result<()> {
    let addr = resolve(host).await?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let endpoint = quic::insecure_client_endpoint(bind).map_err(|e| anyhow!("{e}"))?;
    let conn = endpoint
        .connect(addr, quic::SERVER_NAME)?
        .await
        .with_context(|| format!("connecting to {addr}"))?;

    let (mut ctl_send, mut ctl_recv) = conn.open_bi().await?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        token: token.to_string(),
    };
    quic::send(&mut ctl_send, &hello).await?;
    let welcome: Welcome = match quic::recv(&mut ctl_recv).await {
        Ok(Some(w)) => w,
        _ => match conn.close_reason() {
            Some(reason) => bail!("host refused: {reason}"),
            None => bail!("host closed the connection during the handshake"),
        },
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
    ready
        .send(Ok(Session {
            welcome,
            frames,
            keyframe_requests,
        }))
        .map_err(|_| anyhow!("client went away"))?;

    let requests = tokio::spawn(async move {
        while keyframe_rx.recv().await.is_some() {
            if quic::send(&mut ctl_send, &ClientControl::RequestKeyframe)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let result = loop {
        match quic::recv::<VideoFrame>(&mut video).await {
            Ok(Some(frame)) => {
                if frames_tx.send(frame).is_err() {
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
    requests.abort();
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    result
}
