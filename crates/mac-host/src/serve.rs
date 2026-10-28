//! `aether-agent serve`: stream the main display to one client at a time over
//! QUIC, all in one process. The development mode: run it from a terminal in
//! a logged-in session. The installed setup is `daemon` + `agent`.
//!
//! Capture and encoding start when a client connects and stop when it
//! leaves, so a new client always starts with a keyframe.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use aetherdesk_proto::DEFAULT_PORT;
use aetherdesk_proto::quic;
use aetherdesk_proto::trust::Identity;
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
                }) => {
                    println!("{peer}: {} ({})", c.name, c.fingerprint.short());
                    match client(&conn, send, recv, display, s).await {
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

async fn client(
    conn: &quinn::Connection,
    mut ctl_send: quinn::SendStream,
    ctl_recv: quinn::RecvStream,
    display: &Display,
    settings: &StreamSettings,
) -> anyhow::Result<()> {
    let _awake = crate::power::keep_awake();
    session::welcome(&mut ctl_send, display.width as u32, display.height as u32).await?;
    let mut video = conn.open_uni().await.context("video stream")?;

    let mut stream = Stream::start(display, settings)?;
    let keyframe = stream.keyframe.clone();
    // serve runs in the user's session; nothing to announce.
    let (_notices, notices_rx) = tokio::sync::mpsc::unbounded_channel();
    let control = tokio::spawn(session::read_control(
        ctl_send,
        ctl_recv,
        notices_rx,
        move || keyframe.store(true, Ordering::Relaxed),
    ));
    let injector = stream::spawn_injector(display.bounds);
    let input = tokio::spawn(session::read_input(conn.clone(), move |e| {
        injector.send(e).is_ok()
    }));
    let result = session::send_frames(
        conn,
        &mut video,
        &mut stream.frames,
        Some(stream.skipped.clone()),
    )
    .await;
    control.abort();
    // Dropping the reader drops the injector's sender; it then releases any
    // keys and buttons still held.
    input.abort();
    let stopped = stream.stop();
    result.and(stopped)
}
