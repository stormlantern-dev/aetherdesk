//! The client-facing side of a session, over QUIC: handshake, control
//! messages, input and video. Shared by `serve` and the daemon; what's
//! behind it (a local capture, or an agent) is up to the caller.

use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aetherdesk_proto::trust::{Fingerprint, Role, start_pake};
use aetherdesk_proto::{
    CLOSE_NOT_PAIRED, CLOSE_PAIRING_CLOSED, CLOSE_SHUTDOWN, CLOSE_VERSION, CLOSE_WRONG_CODE,
    ClientControl, DEFAULT_PORT, Hello, HostControl, InputEvent, Intent, MAX_CLIPBOARD,
    PROTOCOL_VERSION, PairMsg, VideoFrame, Welcome, quic,
};
use anyhow::{Context, bail};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::encoder::host_time;
use crate::stream::micros;
use crate::trust::{Client, Store};

/// Runs `accept` (the accept loop) until it ends or the process gets SIGTERM
/// or SIGINT (launchd at shutdown or uninstall, Ctrl+C on `serve`). Then
/// closes every connection with [`CLOSE_SHUTDOWN`] and gives
/// the client a moment to hear it.
pub async fn until_shutdown(
    endpoint: &quinn::Endpoint,
    accept: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let sig = tokio::select! {
        r = accept => return r,
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    println!("{sig}: telling clients the host is shutting down");
    endpoint.close(CLOSE_SHUTDOWN.into(), b"host is shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    Ok(())
}

/// What came of a new connection.
pub enum Admitted {
    /// A paired client wants a session: here is its control stream.
    Session {
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        client: Client,
        /// The client wants clipboard sync.
        clipboard: bool,
    },
    /// A client paired; nothing more to do on this connection.
    Paired(Client),
}

/// Reads the client's hello and decides: a session for a paired client, a
/// pairing while the code is shown, or a refusal. Nothing is streamed and no
/// input is read before this says [`Admitted::Session`]. On refusal the
/// connection is closed with a reason the client shows.
pub async fn admit(
    conn: &quinn::Connection,
    store: &Store,
    host: &Fingerprint,
) -> anyhow::Result<Admitted> {
    let Some(client_fp) = quic::peer_fingerprint(conn) else {
        conn.close(CLOSE_NOT_PAIRED.into(), b"no client certificate");
        bail!("no client certificate");
    };
    let (mut send, mut recv) = conn.accept_bi().await.context("control stream")?;
    let hello: Hello = quic::recv(&mut recv)
        .await?
        .context("client left before saying hello")?;
    if hello.version != PROTOCOL_VERSION {
        conn.close(CLOSE_VERSION.into(), b"protocol version mismatch");
        bail!(
            "client speaks protocol v{}, we speak v{PROTOCOL_VERSION}",
            hello.version
        );
    }
    match hello.intent {
        Intent::Connect { clipboard } => match store.client(&client_fp)? {
            Some(client) => Ok(Admitted::Session {
                send,
                recv,
                client,
                clipboard,
            }),
            None => {
                conn.close(
                    CLOSE_NOT_PAIRED.into(),
                    b"this client isn't paired with the host",
                );
                bail!("client {} isn't paired", client_fp.short());
            }
        },
        Intent::Pair { name } => {
            let Some(pairing) = store.pairing() else {
                conn.close(
                    CLOSE_PAIRING_CLOSED.into(),
                    b"pairing isn't open on the host",
                );
                bail!("{name:?} wants to pair, but pairing isn't open");
            };
            let Some(PairMsg::Spake(theirs)) = quic::recv(&mut recv).await? else {
                bail!("expected the client's pairing message");
            };
            let (pake, ours) = start_pake(&pairing.code);
            quic::send(&mut send, &PairMsg::Spake(ours)).await?;
            let key = pake.finish(&theirs).ok();
            let Some(PairMsg::Confirm(mac)) = quic::recv(&mut recv).await? else {
                bail!("expected the client's confirmation");
            };
            let key = key.filter(|k| k.verify(Role::Client, &client_fp, host, &mac));
            let Some(key) = key else {
                let left = store.pairing_failed();
                conn.close(CLOSE_WRONG_CODE.into(), b"wrong pairing code");
                bail!("{name:?} sent a wrong pairing code ({left} tries left)");
            };
            store.add_client(client_fp, &name)?;
            store.close_pairing();
            let client = store
                .client(&client_fp)?
                .context("client vanished right after pairing")?;
            quic::send(
                &mut send,
                &PairMsg::Paired {
                    name: host_name(),
                    confirm: key.confirm(Role::Host, &client_fp, host),
                },
            )
            .await?;
            let _ = send.finish();
            // Let the client read it and hang up first.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            Ok(Admitted::Paired(client))
        }
    }
}

/// This Mac's name as people know it: the Computer Name from System
/// Settings → General → Sharing (e.g. "Dirk's MacBook Pro"), else the
/// hostname.
pub fn host_name() -> String {
    let computer_name = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty());
    if let Some(name) = computer_name {
        return name;
    }
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = ok
        .then(|| CStr::from_bytes_until_nul(&buf).ok())
        .flatten()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    if name.is_empty() {
        "Mac".into()
    } else {
        name.into()
    }
}

pub async fn welcome(send: &mut quinn::SendStream, width: u32, height: u32) -> anyhow::Result<()> {
    let welcome = Welcome {
        version: PROTOCOL_VERSION,
        width,
        height,
    };
    quic::send(send, &welcome).await?;
    Ok(())
}

/// Handles the control stream until it ends: keyframe requests go to
/// `keyframe`, text the client copied to `clipboard`, pings are answered
/// with the host clock, and `notices` (e.g. [`HostControl::Session`] or
/// the Mac's copies) are passed on to the client.
pub async fn read_control(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut notices: UnboundedReceiver<HostControl>,
    keyframe: impl Fn(),
    clipboard: impl Fn(String),
) {
    // Reading and writing run side by side: a read can't be interrupted
    // halfway through a message without losing the stream.
    let (pongs, mut pongs_rx) = unbounded_channel();
    let reader = async move {
        while let Ok(Some(msg)) = quic::recv::<ClientControl>(&mut recv).await {
            match msg {
                ClientControl::RequestKeyframe => keyframe(),
                ClientControl::Clipboard(text) if text.len() <= MAX_CLIPBOARD => clipboard(text),
                ClientControl::Clipboard(text) => {
                    println!(
                        "  clipboard: skipped {} bytes from the client (too big)",
                        text.len()
                    );
                }
                ClientControl::Ping { client_us } => {
                    let _ = pongs.send(HostControl::Pong {
                        client_us,
                        host_us: micros(host_time()),
                    });
                }
            }
        }
    };
    let writer = async move {
        loop {
            let msg = tokio::select! {
                Some(m) = pongs_rx.recv() => m,
                Some(m) = notices.recv() => m,
                else => break,
            };
            if quic::send(&mut send, &msg).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

/// Passes the client's input events to `event` until the stream ends or
/// `event` returns false.
pub async fn read_input(conn: quinn::Connection, mut event: impl FnMut(InputEvent) -> bool) {
    let Ok(mut stream) = conn.accept_uni().await else {
        return;
    };
    while let Ok(Some(e)) = quic::recv::<InputEvent>(&mut stream).await {
        if !event(e) {
            break;
        }
    }
}

/// Sends frames until the client leaves (`Ok`), the frames run out (`Ok`)
/// or a frame is an error. Prints a stats line every 5 s; `skipped` is the
/// encoder's busy counter, when it runs in this process.
pub async fn send_frames(
    conn: &quinn::Connection,
    video: &mut quinn::SendStream,
    frames: &mut tokio::sync::mpsc::Receiver<Result<VideoFrame, String>>,
    skipped: Option<Arc<AtomicU64>>,
) -> anyhow::Result<()> {
    let mut stats = (0u64, 0u64); // frames, bytes
    let mut next_report = Instant::now() + Duration::from_secs(5);
    loop {
        let item = tokio::select! {
            item = frames.recv() => item,
            _ = conn.closed() => return Ok(()),
        };
        let mut frame = match item {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => bail!(e),
            None => return Ok(()),
        };
        frame.timing.sent_us = micros(host_time());
        stats.0 += 1;
        stats.1 += frame.data.len() as u64;
        if quic::send(video, &frame).await.is_err() {
            return Ok(()); // client went away mid-frame
        }
        if Instant::now() >= next_report {
            let skipped = skipped
                .as_ref()
                .map(|s| format!(" ({} skipped: encoder busy)", s.swap(0, Ordering::Relaxed)))
                .unwrap_or_default();
            println!(
                "  sent {} frames, {:.1} Mbit/s over the last 5 s{skipped}",
                stats.0,
                stats.1 as f64 * 8.0 / 5e6,
            );
            stats = (0, 0);
            next_report += Duration::from_secs(5);
        }
    }
}

/// Prints this Mac's addresses, how to pair, and how many clients are
/// paired. `pair_cmd` is the command that opens pairing for this host.
pub fn print_connect_hint(port: u16, store: &Store, pair_cmd: &str) {
    let port = if port == DEFAULT_PORT {
        String::new()
    } else {
        format!(":{port}")
    };
    let addresses = local_addresses();
    let paired = store.clients().map(|c| c.len()).unwrap_or(0);
    println!(
        "{paired} paired client(s). To pair one: run `{pair_cmd}` here, then on Linux one of:"
    );
    if addresses.is_empty() {
        println!("  aetherdesk pair <this-mac's-ip>{port}");
    }
    for (interface, ip) in &addresses {
        println!("  aetherdesk pair {ip}{port}    ({interface})");
    }
    println!();
}

/// IPv4 addresses of interfaces that are up, without loopback, with the
/// interface name: `en0` is Wi-Fi or Ethernet, `utun*` a VPN such as Tailscale.
pub fn local_addresses() -> Vec<(String, Ipv4Addr)> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut cur = list;
    while let Some(ifa) = unsafe { cur.as_ref() } {
        cur = ifa.ifa_next;
        let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
        let loopback = ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
        let Some(addr) = (unsafe { ifa.ifa_addr.as_ref() }) else {
            continue;
        };
        if !up || loopback || i32::from(addr.sa_family) != libc::AF_INET {
            continue;
        }
        let sin = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        out.push((name, ip));
    }
    unsafe { libc::freeifaddrs(list) };
    out
}
