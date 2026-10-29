//! `aether-agent serve`: stream the main display to one client at a time over
//! QUIC, all in one process. The development mode: run it from a terminal in
//! a logged-in session. The installed setup is `daemon` + `agent`.
//!
//! Capture and encoding start when a client connects and stop when it
//! leaves, so a new client always starts with a keyframe.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use aetherdesk_proto::quic;
use aetherdesk_proto::trust::Identity;
use aetherdesk_proto::{Codec, DEFAULT_PORT, HostControl, picture_size, worth_resizing};
use anyhow::{Context, anyhow};
use objc2_core_graphics::{
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess,
};

use crate::capture::{self, Display};
use crate::encoder::DEFAULT_IN_FLIGHT;
use crate::session::{self, Admitted};
use crate::stream::{self, Stream, StreamSettings};
use crate::trust::Store;
use crate::{cursor, pasteboard};

pub const USAGE: &str = "\
usage: aether-agent serve [--port N] [--fps N] [--mbps N] [--in-flight N]

Only paired clients get in; pair them with `aether-agent pair --serve`.
serve keeps its own identity and paired clients in
~/Library/Application Support/AetherDesk/serve, apart from the installed host.

  --port N    UDP port to listen on (default 4500)
  --fps N     maximum frame rate (default 60)
  --mbps N    average bitrate in Mbit/s (default 20)
  --in-flight N
              frames the encoder may work on at once (default 1); 2 gives
              a higher frame rate with older frames";

/// Options `serve` and `daemon` share.
pub struct Opts {
    pub port: u16,
    pub stream: StreamSettings,
}

/// Parses the shared options; `extra` gets every other flag with a way to
/// read its value, and returns false for flags it doesn't know either.
pub fn parse(
    args: &[String],
    usage: &str,
    mut extra: impl FnMut(&str, &mut dyn FnMut() -> Result<String, String>) -> Result<bool, String>,
) -> Result<Opts, String> {
    let mut o = Opts {
        port: DEFAULT_PORT,
        stream: StreamSettings {
            fps: 60,
            mbps: 20,
            in_flight: DEFAULT_IN_FLIGHT,
        },
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        let num = |v: String| v.parse::<u32>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--port" => o.port = val()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--fps" => o.stream.fps = num(val()?)?.max(1),
            "--mbps" => o.stream.mbps = num(val()?)?.max(1),
            "--in-flight" => o.stream.in_flight = num(val()?)?.max(1) as usize,
            "-h" | "--help" => return Err(usage.into()),
            other => {
                if !extra(other, &mut val)? {
                    return Err(format!("unknown option {other}\n\n{usage}"));
                }
            }
        }
    }
    Ok(o)
}

/// Checks Screen Recording and Accessibility for this process, asking macOS
/// to show the prompt when one is missing. Without Screen Recording there is
/// nothing to stream; without Accessibility only input is lost.
pub fn check_permissions(who: &str) -> Result<(), String> {
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        return Err(format!(
            "no Screen Recording permission. Grant it to {who} in System Settings → \
             Privacy & Security → Screen & System Audio Recording, then restart it."
        ));
    }
    if !CGPreflightPostEventAccess() {
        CGRequestPostEventAccess();
        eprintln!(
            "warning: no Accessibility permission, so keyboard and mouse from the client \
             won't work. Grant it to {who} in System Settings → Privacy & Security → \
             Accessibility, then restart it.\n"
        );
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let o = parse(args, USAGE, |_, _| Ok(false))?;
    check_permissions("the app you run this from (e.g. your terminal)")?;
    let store = Store::serve()?;
    let id = store
        .identity()
        .map_err(|e| format!("identity in {}: {e}", store.dir().display()))?;
    let display = capture::main_display()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime
        .block_on(serve(&o, &display, &store, &id))
        .map_err(|e| format!("{e:#}"))
}

async fn serve(o: &Opts, display: &Display, store: &Store, id: &Identity) -> anyhow::Result<()> {
    let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], o.port)), id)
        .map_err(|e| anyhow!("listening on UDP port {}: {e}", o.port))?;
    let s = &o.stream;
    println!(
        "serving main display {}x{} px on UDP port {} (up to {} fps, {} Mbit/s, {} in flight)",
        display.width, display.height, o.port, s.fps, s.mbps, s.in_flight
    );
    session::print_connect_hint(o.port, store, "aether-agent pair --serve");
    let host = id.fingerprint();

    // One client at a time: others wait in the accept queue.
    let accept = async {
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
            match session::admit(&conn, store, &host).await {
                Ok(Admitted::Session {
                    send,
                    recv,
                    client: c,
                    clipboard,
                    codec,
                    audio,
                }) => {
                    println!("{peer}: {} ({}), {codec}", c.name, c.fingerprint.short());
                    let wants = Wants {
                        clipboard,
                        codec,
                        audio,
                    };
                    match client(&conn, send, recv, display, s, wants).await {
                        Ok(()) => println!("{peer}: disconnected"),
                        Err(e) => eprintln!("{peer}: {e:#}"),
                    }
                }
                Ok(Admitted::Paired(c)) => println!("{peer}: paired with {}", c.name),
                Err(e) => eprintln!("{peer}: {e:#}"),
            }
            conn.close(0u32.into(), b"bye");
        }
        Ok(())
    };
    session::until_shutdown(&endpoint, accept).await
}

/// What the client asked for.
struct Wants {
    clipboard: bool,
    codec: Codec,
    audio: bool,
}

async fn client(
    conn: &quinn::Connection,
    mut ctl_send: quinn::SendStream,
    ctl_recv: quinn::RecvStream,
    display: &Display,
    settings: &StreamSettings,
    wants: Wants,
) -> anyhow::Result<()> {
    let Wants {
        clipboard,
        codec,
        audio,
    } = wants;
    let _awake = crate::power::keep_awake();
    session::welcome(
        &mut ctl_send,
        display.width as u32,
        display.height as u32,
        codec,
    )
    .await?;
    let mut video = conn.open_uni().await.context("video stream")?;

    let local_pointer = cursor::available();
    let full = (display.width as u32, display.height as u32);
    let mut size = picture_size(full, None);
    let start = |size: (u32, u32)| {
        Stream::start(
            display,
            (size.0 as usize, size.1 as usize),
            settings,
            !local_pointer,
            codec,
        )
    };
    let mut stream = start(size)?;
    // Frames of whichever stream is current go out through one channel, so
    // a resize (a new stream) doesn't touch the sending.
    let (frames_tx, mut frames) = tokio::sync::mpsc::channel(4);
    let mut pump = forward(&mut stream, frames_tx.clone());
    let keyframe = Arc::new(std::sync::Mutex::new(stream.keyframe.clone()));
    let (views, mut view_rx) = tokio::sync::mpsc::unbounded_channel();
    // serve runs in the user's session: no session changes to announce,
    // only the Mac's copies.
    let (notices, notices_rx) = tokio::sync::mpsc::unbounded_channel();
    let last_move = cursor::LastMove::default();
    let _pointer = local_pointer.then(|| {
        let notices = notices.clone();
        cursor::Watcher::start(display.bounds, last_move.clone(), move |msg| {
            let _ = notices.send(msg);
        })
    });
    let pasteboard = clipboard.then(|| {
        Arc::new(pasteboard::Watcher::start(move |text| {
            let _ = notices.send(HostControl::Clipboard(text));
        }))
    });
    let to_pasteboard = pasteboard.clone();
    let current_keyframe = keyframe.clone();
    let control = tokio::spawn(session::read_control(
        ctl_send,
        ctl_recv,
        notices_rx,
        move || {
            current_keyframe
                .lock()
                .unwrap()
                .store(true, Ordering::Relaxed)
        },
        move |text| {
            if let Some(p) = &to_pasteboard {
                p.set(text);
            }
        },
        move |width, height| {
            let _ = views.send((width, height));
        },
    ));
    // No sound is no reason to end the session.
    let sound = audio
        .then(|| {
            let conn = conn.clone();
            crate::audio::AudioCapture::start(
                display,
                Box::new(move |p| session::send_audio(&conn, &p)),
            )
            .inspect(|_| println!("  sound: capturing"))
            .inspect_err(|e| eprintln!("  no sound: {e}"))
            .ok()
        })
        .flatten();
    let injector = stream::spawn_injector(display.bounds, last_move);
    let input = tokio::spawn(session::read_input(conn.clone(), move |e| {
        injector.send(e).is_ok()
    }));
    let sending = session::send_frames(conn, &mut video, &mut frames, None);
    // The client's window changed: capture at the size that fits it. The
    // new stream starts first; if it can't, the old one goes on.
    let resizing = async {
        while let Some(view) = view_rx.recv().await {
            let wanted = picture_size(full, Some(view));
            if !worth_resizing(size, wanted) {
                continue;
            }
            match start(wanted) {
                Ok(mut new) => {
                    pump.abort();
                    *keyframe.lock().unwrap() = new.keyframe.clone();
                    pump = forward(&mut new, frames_tx.clone());
                    let old = std::mem::replace(&mut stream, new);
                    if let Err(e) = old.stop() {
                        eprintln!("stopping capture: {e:#}");
                    }
                    println!(
                        "  picture {}x{} → {}x{} (window {}x{})",
                        size.0, size.1, wanted.0, wanted.1, view.0, view.1
                    );
                    size = wanted;
                }
                Err(e) => eprintln!("  can't capture at {}x{}: {e:#}", wanted.0, wanted.1),
            }
        }
        std::future::pending::<()>().await;
    };
    let result = tokio::select! {
        r = sending => r,
        () = resizing => Ok(()),
    };
    control.abort();
    pump.abort();
    // Dropping the reader drops the injector's sender; it then releases any
    // keys and buttons still held.
    input.abort();
    if let Some(s) = sound {
        s.stop();
    }
    let stopped = stream.stop();
    result.and(stopped)
}

/// Passes `stream`'s frames on to `out`, and says every 5 s how many
/// captures the busy encoder skipped.
fn forward(
    stream: &mut Stream,
    out: tokio::sync::mpsc::Sender<Result<aetherdesk_proto::VideoFrame, String>>,
) -> tokio::task::JoinHandle<()> {
    let (_, mut frames) = tokio::sync::mpsc::channel(1);
    std::mem::swap(&mut frames, &mut stream.frames);
    let skipped = stream.skipped.clone();
    tokio::spawn(async move {
        let mut report = tokio::time::interval(std::time::Duration::from_secs(5));
        report.tick().await;
        loop {
            tokio::select! {
                item = frames.recv() => match item {
                    Some(item) => {
                        if out.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = report.tick() => {
                    let n = skipped.swap(0, Ordering::Relaxed);
                    if n > 0 {
                        println!("  {n} captures skipped in 5 s: encoder busy");
                    }
                }
            }
        }
    })
}
