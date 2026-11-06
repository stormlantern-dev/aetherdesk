//! The client-facing side of a session, over QUIC: handshake, control
//! messages, input and video. Shared by `serve` and the daemon; what's
//! behind it (a local capture, or an agent) is up to the caller.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aetherdesk_proto::trust::{Fingerprint, Role, start_pake};
use aetherdesk_proto::{
    AudioPacket, CLOSE_CODEC, CLOSE_NOT_PAIRED, CLOSE_PAIRING_CLOSED, CLOSE_SHUTDOWN,
    CLOSE_VERSION, CLOSE_WRONG_CODE, ClientControl, Codec, DEFAULT_PORT, Hello, HostControl,
    InputEvent, Intent, MAX_CLIPBOARD, PROTOCOL_VERSION, PairMsg, VideoFrame, Welcome, quic,
};
use anyhow::{Context, bail};
use tokio::sync::mpsc::{self, UnboundedReceiver, unbounded_channel};

use crate::encoder::host_time;
use crate::stream::micros;
use crate::trust::{Client, Store};

/// The video formats this host sends. The client's order decides.
pub const CODECS: &[Codec] = &[Codec::Hevc444, Codec::Hevc, Codec::H264];

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

/// How long a client's QUIC handshake may take, and then its hello.
const HANDSHAKE: Duration = Duration::from_secs(5);

/// Completes incoming handshakes side by side, each within [`HANDSHAKE`],
/// and passes on the connections that made it, in the order they did.
///
/// Not one after the other in the accept loop: after an outage a client
/// leaves handshakes behind that it gave up on (it starts a fresh one every
/// so often), and each would hold up the loop until it timed out, with the
/// one that works waiting behind them. And while a session runs, a client
/// that comes back on a new connection is seen at once ([`serve_one`]).
pub fn handshakes(endpoint: &quinn::Endpoint) -> mpsc::Receiver<quinn::Connection> {
    let (tx, rx) = mpsc::channel(16);
    let endpoint = endpoint.clone();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let peer = incoming.remote_address();
                let connecting = match incoming.accept() {
                    Ok(c) => c,
                    Err(e) => return eprintln!("{peer}: handshake failed: {e}"),
                };
                match tokio::time::timeout(HANDSHAKE, connecting).await {
                    Ok(Ok(conn)) => {
                        let _ = tx.send(conn).await;
                    }
                    Ok(Err(e)) => eprintln!("{peer}: handshake failed: {e}"),
                    Err(_) => eprintln!(
                        "{peer}: handshake: no answer within {} s",
                        HANDSHAKE.as_secs()
                    ),
                }
            });
        }
    });
    rx
}

/// The next connection to serve: one that waited during the last session,
/// else a new one. `None` once the endpoint is closed.
pub async fn next_connection(
    conns: &mut mpsc::Receiver<quinn::Connection>,
    waiting: &mut VecDeque<quinn::Connection>,
) -> Option<quinn::Connection> {
    loop {
        let conn = match waiting.pop_front() {
            Some(c) => c,
            None => conns.recv().await?,
        };
        // A client that gave up while it waited.
        if conn.close_reason().is_none() {
            return Some(conn);
        }
    }
}

/// [`admit`], within [`HANDSHAKE`]: a client that connects and says
/// nothing doesn't hold up the others.
pub async fn admit_soon(
    conn: &quinn::Connection,
    store: &Store,
    host: &Fingerprint,
) -> anyhow::Result<Admitted> {
    match tokio::time::timeout(HANDSHAKE, admit(conn, store, host)).await {
        Ok(r) => r,
        Err(_) => {
            conn.close(0u32.into(), b"no hello");
            bail!("no hello within {} s", HANDSHAKE.as_secs())
        }
    }
}

/// Runs `session`, the session of the client with certificate `client` on
/// `conn`, one client at a time: other clients' new connections wait in
/// `waiting`. When the same client connects again meanwhile, its old
/// connection is dead to it (it gave up after an outage, or restarted), so
/// `conn` is closed to end the session at once, rather than after the idle
/// timeout, and the new connection goes first. Returns the session's result
/// and whether it was replaced that way.
pub async fn serve_one<T>(
    conn: &quinn::Connection,
    client: Fingerprint,
    conns: &mut mpsc::Receiver<quinn::Connection>,
    waiting: &mut VecDeque<quinn::Connection>,
    session: impl Future<Output = T>,
) -> (T, bool) {
    tokio::pin!(session);
    let mut replaced = false;
    let mut open = true;
    loop {
        tokio::select! {
            r = &mut session => return (r, replaced),
            c = conns.recv(), if open => match c {
                Some(new) if !replaced && quic::peer_fingerprint(&new) == Some(client) => {
                    println!(
                        "{}: the client is back from {}; ending the old connection",
                        conn.remote_address(),
                        new.remote_address()
                    );
                    conn.close(0u32.into(), b"replaced by a new connection");
                    waiting.push_front(new);
                    replaced = true;
                }
                Some(other) => waiting.push_back(other),
                None => open = false,
            },
        }
    }
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
        /// The video format both sides agreed on.
        codec: Codec,
        /// The client wants the Mac's sound.
        audio: bool,
        /// The client offers its microphone.
        mic: bool,
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
        Intent::Connect {
            clipboard,
            codecs,
            audio,
            mic,
        } => match store.client(&client_fp)? {
            Some(client) => {
                let Some(codec) = Codec::pick(&codecs, CODECS) else {
                    conn.close(CLOSE_CODEC.into(), b"no video format in common");
                    bail!("client decodes {codecs:?}, we send {CODECS:?}: nothing in common");
                };
                Ok(Admitted::Session {
                    send,
                    recv,
                    client,
                    clipboard,
                    codec,
                    audio,
                    mic,
                })
            }
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

pub async fn welcome(
    send: &mut quinn::SendStream,
    width: u32,
    height: u32,
    codec: Codec,
    mic: bool,
) -> anyhow::Result<()> {
    let welcome = Welcome {
        version: PROTOCOL_VERSION,
        width,
        height,
        codec,
        mic,
    };
    quic::send(send, &welcome).await?;
    Ok(())
}

/// Handles the control stream until it ends: keyframe requests go to
/// `keyframe`, text the client copied to `clipboard`, its window size to
/// `view_size`, pings are answered with the host clock, and `notices` (e.g.
/// [`HostControl::Session`] or the Mac's copies) are passed on to the
/// client.
pub async fn read_control(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut notices: UnboundedReceiver<HostControl>,
    keyframe: impl Fn(),
    clipboard: impl Fn(String),
    view_size: impl Fn(u32, u32),
) {
    // Reading and writing run side by side: a read can't be interrupted
    // halfway through a message without losing the stream.
    let (pongs, mut pongs_rx) = unbounded_channel();
    let reader = async move {
        while let Ok(Some(msg)) = quic::recv::<ClientControl>(&mut recv).await {
            match msg {
                ClientControl::RequestKeyframe => keyframe(),
                ClientControl::ViewSize { width, height } => view_size(width, height),
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

/// Frames the daemon dropped because the client's queue was full (the
/// network or the client is behind); reported in the "sent" line.
pub static DROPPED_BEHIND: AtomicU64 = AtomicU64::new(0);

/// Sends frames until the client leaves (`Ok`), the frames run out (`Ok`)
/// or a frame is an error. Prints a stats line every 5 s; `skipped` is the
/// encoder's busy counter, when it runs in this process.
pub async fn send_frames(
    conn: &quinn::Connection,
    video: &mut quinn::SendStream,
    frames: &mut tokio::sync::mpsc::Receiver<Result<VideoFrame, String>>,
    skipped: Option<Arc<AtomicU64>>,
) -> anyhow::Result<()> {
    let mut stats = crate::pipeline::Sent::default();
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
        let bytes = frame.data.len();
        let writing = Instant::now();
        if quic::send(video, &frame).await.is_err() {
            return Ok(()); // client went away mid-frame
        }
        stats.frame(bytes, writing.elapsed().as_micros() as i64);
        if Instant::now() >= next_report {
            let skipped = skipped
                .as_ref()
                .map(|s| format!(" ({} skipped: encoder busy)", s.swap(0, Ordering::Relaxed)))
                .unwrap_or_default();
            let path = conn.stats().path;
            println!(
                "  sent {} frames, {:.1} Mbit/s over the last 5 s{skipped}, {} dropped (client behind) | {}",
                stats.frames(),
                stats.mbps(5.0),
                DROPPED_BEHIND.swap(0, Ordering::Relaxed),
                stats.line(crate::pipeline::Path {
                    rtt_us: path.rtt.as_micros() as i64,
                    cwnd: path.cwnd,
                    lost_packets: path.lost_packets,
                    congestion_events: path.congestion_events,
                }),
            );
            next_report += Duration::from_secs(5);
        }
    }
}

/// Sends one sound packet as a datagram. A packet that can't go now (the
/// network is behind, or the client is gone) is dropped: late sound is
/// worse than a short gap.
pub fn send_audio(conn: &quinn::Connection, packet: &AudioPacket) {
    if let Ok(bytes) = packet.to_datagram() {
        let _ = conn.send_datagram(bytes.into());
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use aetherdesk_proto::quic::HostTrust;
    use aetherdesk_proto::trust::Identity;

    use super::*;

    async fn connect(ep: &quinn::Endpoint, to: SocketAddr) -> quinn::Connection {
        ep.connect(to, quic::SERVER_NAME).unwrap().await.unwrap()
    }

    /// A client that reconnects (after giving up on a dead link) ends its
    /// old session at once; another client waits its turn.
    #[tokio::test]
    async fn a_new_connection_from_the_same_client_replaces_the_old() {
        let host = Identity::generate().unwrap();
        let (me, other) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = quic::server_endpoint(local, &host).unwrap();
        let addr = server.local_addr().unwrap();
        let pinned = || HostTrust::Pinned(host.fingerprint());
        let mine = quic::client_endpoint(local, &me, pinned()).unwrap();
        let theirs = quic::client_endpoint(local, &other, pinned()).unwrap();

        let mut conns = handshakes(&server);
        let mut waiting = VecDeque::new();
        let _first = connect(&mine, addr).await;
        let old = next_connection(&mut conns, &mut waiting).await.unwrap();

        let session = {
            let old = old.clone();
            async move { old.closed().await }
        };
        let (_other, _again) = tokio::join!(connect(&theirs, addr), async {
            // After the other client, so it's queued first.
            tokio::time::sleep(Duration::from_millis(200)).await;
            connect(&mine, addr).await
        });
        let (_, replaced) = tokio::time::timeout(
            Duration::from_secs(5),
            serve_one(&old, me.fingerprint(), &mut conns, &mut waiting, session),
        )
        .await
        .expect("the old session should end");
        assert!(replaced);
        let next = next_connection(&mut conns, &mut waiting).await.unwrap();
        assert_eq!(quic::peer_fingerprint(&next), Some(me.fingerprint()));
        let after = next_connection(&mut conns, &mut waiting).await.unwrap();
        assert_eq!(quic::peer_fingerprint(&after), Some(other.fingerprint()));
    }
}
