//! `aether-agent serve`: stream the main display to one client at a time over
//! QUIC. Capture and encoding start when a client connects and stop when it
//! leaves, so a new client always starts with a keyframe.

use std::ffi::CStr;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aetherdesk_proto::quic;
use aetherdesk_proto::{
    ClientControl, DEFAULT_PORT, Hello, HostControl, HostTiming, InputEvent, PROTOCOL_VERSION,
    VideoFrame, Welcome,
};
use anyhow::{Context, anyhow, bail};
use objc2_core_graphics::{
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess,
};
use tokio::sync::mpsc::error::TrySendError;

use crate::capture::{self, Capture, Display};
use crate::encoder::{DEFAULT_IN_FLIGHT, EncodedFrame, Encoder, Settings, host_time};
use crate::inject;

pub const USAGE: &str = "\
usage: aether-agent serve [--port N] [--token T] [--fps N] [--mbps N] [--in-flight N]

  --port N    UDP port to listen on (default 4500)
  --token T   secret clients must send (default: $AETHERDESK_TOKEN, else random
              and printed at start)
  --fps N     maximum frame rate (default 60)
  --mbps N    average bitrate in Mbit/s (default 20)
  --in-flight N
              frames the encoder may work on at once (default 2); 1 gives
              fresher frames, possibly at a lower frame rate";

/// Frames waiting to be sent. Beyond this the client or network is too slow:
/// we drop frames and ask the encoder for a keyframe to resync.
const SEND_QUEUE: usize = 8;

struct Opts {
    port: u16,
    token: Option<String>,
    fps: u32,
    mbps: u32,
    in_flight: usize,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        port: DEFAULT_PORT,
        token: None,
        fps: 60,
        mbps: 20,
        in_flight: DEFAULT_IN_FLIGHT,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        let num = |v: String| v.parse::<u32>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--port" => o.port = val()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--token" => o.token = Some(val()?),
            "--fps" => o.fps = num(val()?)?.max(1),
            "--mbps" => o.mbps = num(val()?)?.max(1),
            "--in-flight" => o.in_flight = num(val()?)?.max(1) as usize,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown option {flag}\n\n{USAGE}")),
        }
    }
    Ok(o)
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| format!("random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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

pub fn run(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        return Err(
            "no Screen Recording permission. Grant it to the app you run this from \
                    (e.g. your terminal) in System Settings → Privacy & Security → Screen & \
                    System Audio Recording, restart that app, and try again."
                .into(),
        );
    }
    if !CGPreflightPostEventAccess() {
        CGRequestPostEventAccess();
        eprintln!(
            "warning: no Accessibility permission, so keyboard and mouse from the client \
             won't work. Grant it to the app you run this from (e.g. your terminal) in System \
             Settings → Privacy & Security → Accessibility, then restart that app.\n"
        );
    }
    let token = match &o.token {
        Some(t) => t.clone(),
        None => match std::env::var("AETHERDESK_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => random_token()?,
        },
    };
    let display = capture::main_display()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime
        .block_on(serve(&o, &display, &token))
        .map_err(|e| format!("{e:#}"))
}

async fn serve(o: &Opts, display: &Display, token: &str) -> anyhow::Result<()> {
    let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], o.port)))
        .map_err(|e| anyhow!("listening on UDP port {}: {e}", o.port))?;
    println!(
        "serving main display {}x{} px on UDP port {} (up to {} fps, {} Mbit/s, {} in flight)",
        display.width, display.height, o.port, o.fps, o.mbps, o.in_flight
    );
    let port = if o.port == DEFAULT_PORT {
        String::new()
    } else {
        format!(":{}", o.port)
    };
    let addresses = local_addresses();
    println!("connect from Linux with one of:");
    if addresses.is_empty() {
        println!("  aetherdesk <this-mac's-ip>{port} --token {token}");
    }
    for (interface, ip) in &addresses {
        println!("  aetherdesk {ip}{port} --token {token}    ({interface})");
    }
    println!();

    // One client at a time: others wait in the accept queue.
    while let Some(incoming) = endpoint.accept().await {
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("handshake failed: {e}");
                continue;
            }
        };
        let peer = conn.remote_address();
        println!("{peer}: connected");
        match session(&conn, display, token, o).await {
            Ok(()) => println!("{peer}: disconnected"),
            Err(e) => eprintln!("{peer}: {e:#}"),
        }
        conn.close(0u32.into(), b"bye");
    }
    Ok(())
}

async fn session(
    conn: &quinn::Connection,
    display: &Display,
    token: &str,
    o: &Opts,
) -> anyhow::Result<()> {
    let (mut ctl_send, mut ctl_recv) = conn.accept_bi().await.context("control stream")?;
    let hello: Hello = quic::recv(&mut ctl_recv)
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
    let welcome = Welcome {
        version: PROTOCOL_VERSION,
        width: display.width as u32,
        height: display.height as u32,
    };
    quic::send(&mut ctl_send, &welcome).await?;
    let mut video = conn.open_uni().await.context("video stream")?;

    let (tx, rx) = mpsc::channel();
    let settings = Settings {
        width: display.width,
        height: display.height,
        fps: o.fps,
        bitrate_bps: o.mbps.saturating_mul(1_000_000),
        max_in_flight: o.in_flight,
    };
    let encoder = Encoder::new(&settings, tx.clone()).map_err(anyhow::Error::msg)?;
    let keyframe = encoder.keyframe_trigger();
    let skipped = encoder.skipped_counter();
    let capture = Capture::start(display, o.fps, encoder, tx).map_err(anyhow::Error::msg)?;

    let frames = forward_frames(rx, keyframe.clone());
    let control = tokio::spawn(read_control(ctl_send, ctl_recv, keyframe));
    let input = tokio::spawn(read_input(conn.clone(), display.bounds));
    let result = send_frames(conn, &mut video, frames, skipped).await;
    control.abort();
    // Dropping the reader closes the injector's channel; it then releases
    // any keys and buttons still held.
    input.abort();
    let stopped = capture.stop().map_err(anyhow::Error::msg);
    result.and(stopped)
}

/// Moves encoder output from its std channel onto a small bounded tokio
/// channel. When that's full the network is behind: drop the frame and
/// force a keyframe, since every following P-frame depends on the lost one.
fn forward_frames(
    rx: mpsc::Receiver<Result<EncodedFrame, String>>,
    keyframe: Arc<AtomicBool>,
) -> tokio::sync::mpsc::Receiver<Result<EncodedFrame, String>> {
    let (tx, out) = tokio::sync::mpsc::channel(SEND_QUEUE);
    std::thread::spawn(move || {
        for item in rx {
            match item {
                Ok(frame) => match tx.try_send(Ok(frame)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => keyframe.store(true, Ordering::Relaxed),
                    Err(TrySendError::Closed(_)) => break,
                },
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                }
            }
        }
    });
    out
}

/// Reads input events from the client's input stream and hands them to an
/// injector thread, which owns the CoreGraphics event source.
async fn read_input(conn: quinn::Connection, bounds: crate::input::Rect) {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::spawn(move || inject::run(bounds, rx));
    let Ok(mut stream) = conn.accept_uni().await else {
        return;
    };
    while let Ok(Some(event)) = quic::recv::<InputEvent>(&mut stream).await {
        if tx.send(event).is_err() {
            break;
        }
    }
}

async fn read_control(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    keyframe: Arc<AtomicBool>,
) {
    while let Ok(Some(msg)) = quic::recv::<ClientControl>(&mut recv).await {
        match msg {
            ClientControl::RequestKeyframe => keyframe.store(true, Ordering::Relaxed),
            ClientControl::Ping { client_us } => {
                let pong = HostControl::Pong {
                    client_us,
                    host_us: micros(host_time()),
                };
                if quic::send(&mut send, &pong).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn micros(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1e6) as u64
}

async fn send_frames(
    conn: &quinn::Connection,
    video: &mut quinn::SendStream,
    mut frames: tokio::sync::mpsc::Receiver<Result<EncodedFrame, String>>,
    skipped: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let mut first_pts = None;
    let mut stats = (0u64, 0u64); // frames, bytes
    let mut next_report = Instant::now() + Duration::from_secs(5);
    loop {
        let item = tokio::select! {
            item = frames.recv() => item,
            _ = conn.closed() => return Ok(()),
        };
        let frame = match item {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => bail!(e),
            None => return Ok(()),
        };
        let base = *first_pts.get_or_insert(frame.pts);
        let msg = VideoFrame {
            pts_us: ((frame.pts - base).max(0.0) * 1e6) as u64,
            keyframe: frame.keyframe,
            timing: HostTiming {
                captured_us: micros(frame.pts),
                encoded_us: micros(frame.encoded_at),
                sent_us: micros(host_time()),
            },
            data: frame.data,
        };
        stats.0 += 1;
        stats.1 += msg.data.len() as u64;
        if quic::send(video, &msg).await.is_err() {
            return Ok(()); // client went away mid-frame
        }
        if Instant::now() >= next_report {
            println!(
                "  sent {} frames, {:.1} Mbit/s over the last 5 s ({} skipped: encoder busy)",
                stats.0,
                stats.1 as f64 * 8.0 / 5e6,
                skipped.swap(0, Ordering::Relaxed)
            );
            stats = (0, 0);
            next_report += Duration::from_secs(5);
        }
    }
}
