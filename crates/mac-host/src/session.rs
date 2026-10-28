//! The client-facing side of a session, over QUIC: handshake, control
//! messages, input and video. Shared by `serve` and the daemon; what's
//! behind it (a local capture, or an agent) is up to the caller.

use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aetherdesk_proto::quic;
use aetherdesk_proto::{
    ClientControl, DEFAULT_PORT, Hello, HostControl, InputEvent, PROTOCOL_VERSION, VideoFrame,
    Welcome,
};
use anyhow::{Context, bail};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::encoder::host_time;
use crate::stream::micros;

/// Reads the client's hello and checks version and token. On success the
/// control stream is returned; on failure the connection is closed with a
/// reason the client shows.
pub async fn handshake(
    conn: &quinn::Connection,
    token: &str,
) -> anyhow::Result<(quinn::SendStream, quinn::RecvStream)> {
    let (send, mut recv) = conn.accept_bi().await.context("control stream")?;
    let hello: Hello = quic::recv(&mut recv)
        .await?
        .context("client left before saying hello")?;
    if hello.version != PROTOCOL_VERSION {
        conn.close(1u32.into(), b"protocol version mismatch");
        bail!(
            "client speaks protocol v{}, we speak v{PROTOCOL_VERSION}",
            hello.version
        );
    }
    if hello.token != token {
        conn.close(2u32.into(), b"wrong token");
        bail!("wrong token");
    }
    Ok((send, recv))
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
/// `keyframe`, pings are answered with the host clock, and `notices` (e.g.
/// [`HostControl::Session`]) are passed on to the client.
pub async fn read_control(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut notices: UnboundedReceiver<HostControl>,
    keyframe: impl Fn(),
) {
    // Reading and writing run side by side: a read can't be interrupted
    // halfway through a message without losing the stream.
    let (pongs, mut pongs_rx) = unbounded_channel();
    let reader = async move {
        while let Ok(Some(msg)) = quic::recv::<ClientControl>(&mut recv).await {
            match msg {
                ClientControl::RequestKeyframe => keyframe(),
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

pub fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| format!("random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Prints how to connect from Linux, with this Mac's addresses.
pub fn print_connect_hint(port: u16, token: Option<&str>) {
    let port = if port == DEFAULT_PORT {
        String::new()
    } else {
        format!(":{port}")
    };
    let token = token.map_or(String::new(), |t| format!(" --token {t}"));
    let addresses = local_addresses();
    println!("connect from Linux with one of:");
    if addresses.is_empty() {
        println!("  aetherdesk <this-mac's-ip>{port}{token}");
    }
    for (interface, ip) in &addresses {
        println!("  aetherdesk {ip}{port}{token}    ({interface})");
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
