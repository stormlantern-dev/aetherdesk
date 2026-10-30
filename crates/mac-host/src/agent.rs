//! `aether-agent agent`: runs in a GUI session as a LaunchAgent — the login
//! window (as root) or a logged-in user's session. It connects to the
//! daemon and captures, encodes and injects when the daemon says so.
//! Screen Recording and Accessibility belong to this binary here, not to a
//! terminal.

use std::sync::atomic::Ordering;
use std::time::Duration;

use aetherdesk_proto::{Codec, HostControl, InputEvent, picture_size, worth_resizing};
use anyhow::Context;
use objc2_core_graphics::CGPreflightPostEventAccess;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio::AudioCapture;
use crate::capture;
use crate::ipc::{self, FromAgent, ToAgent};
use crate::serve;
use crate::stream::{self, Stream, StreamSettings};
use crate::{cursor, pasteboard};

pub const USAGE: &str = "\
usage: aether-agent agent [--socket PATH]

Runs in a GUI session (LaunchAgent) and waits for the daemon to ask for
the screen.

  --socket PATH  the daemon's agent socket (default /var/run/aetherdesk/agent.sock)";

/// How long to wait for a display that is just waking up.
const DISPLAY_WAIT: Duration = Duration::from_secs(5);

/// Wait between attempts to reach the daemon.
const RETRY: Duration = Duration::from_secs(2);

/// Exit status when the login window's agent gives up on its process, so
/// launchd (KeepAlive) starts a fresh one. EX_TEMPFAIL.
const RESTART_STATUS: i32 = 75;

/// Raised when a running capture fails. In the login window that process
/// won't capture again (as with a failed start), so the agent restarts.
static CAPTURE_LOST: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

pub fn run(args: &[String]) -> Result<(), String> {
    let mut socket = ipc::SOCKET.to_string();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--socket" => socket = it.next().cloned().ok_or("--socket needs a value")?,
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    let uid = unsafe { libc::getuid() };
    let session = if uid == 0 {
        "login window"
    } else {
        "user session"
    };
    println!(
        "agent {} in the {session} (uid {uid}, pid {})",
        ipc::VERSION,
        std::process::id()
    );
    // Not fatal: the permission may be granted while we run, and the
    // daemon hears about it when capture fails.
    if let Err(e) = serve::check_permissions("this agent (aether-agent)") {
        eprintln!("warning: {e}");
    }

    crate::mute::recover();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime.block_on(async {
        // launchd stops the agent with SIGTERM when its session ends (the
        // login window's at login, the user's at logout), possibly while
        // the Mac is muted for a client. Unmute before going.
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| format!("SIGTERM handler: {e}"))?;
        tokio::select! {
            r = serve_forever(&socket) => r,
            _ = term.recv() => {
                println!("SIGTERM: stopping");
                crate::mute::recover();
                // Not waiting for the capture to wind down: the session is
                // going away anyway.
                std::process::exit(0);
            }
        }
    })
}

/// Serves the daemon, reconnecting while it's down. Never returns: a
/// restart exits the process.
async fn serve_forever(socket: &str) -> Result<(), String> {
    let mut reported = false;
    loop {
        match serve_daemon(socket).await {
            Ok(Ended::Restart) => {
                println!("exiting so launchd starts a fresh agent for the login window");
                std::process::exit(RESTART_STATUS);
            }
            Ok(Ended::Closed) => println!("daemon closed the connection"),
            // Say it once, not every 2 s while the daemon is down.
            Err(e) if !reported => {
                eprintln!("{e:#}; retrying every {} s", RETRY.as_secs());
                reported = true;
            }
            Err(_) => {}
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Why [`serve_daemon`] returned.
enum Ended {
    /// The daemon closed the connection.
    Closed,
    /// Capture failed at the login window. On macOS 27 a capture started
    /// moments after logout fails ("Could not obtain content filter"), and
    /// that process can't capture again, not even later ("no display
    /// mode"); a fresh process can. So this one makes way.
    Restart,
}

/// The capture the daemon asked for.
struct Active {
    stream: Stream,
    pump: JoinHandle<()>,
    /// What a resize needs to start the stream again.
    display: capture::Display,
    settings: StreamSettings,
    shows_cursor: bool,
    codec: Codec,
    injector: std::sync::mpsc::Sender<InputEvent>,
    /// Input events passed to the injector, for the log.
    inputs: u64,
    /// The session's clipboard, when the client asked for it.
    pasteboard: Option<pasteboard::Watcher>,
    /// Sends the pointer's shape, when the client draws it.
    _pointer: Option<cursor::Watcher>,
    /// The session's sound, when the client wants it.
    sound: Option<AudioCapture>,
    /// The Mac's own output, muted while its sound plays on the client.
    /// Unmuted when dropped, after the capture stopped.
    _muted: Option<crate::mute::Muted>,
}

impl Active {
    /// The client's window is now `view` pixels: capture at the size that
    /// fits it, if that's different enough to be worth a keyframe.
    fn view_size(&mut self, view: (u32, u32), out: &mpsc::UnboundedSender<FromAgent>) {
        let display = (self.display.width as u32, self.display.height as u32);
        let wanted = picture_size(display, Some(view));
        let now = (self.stream.width, self.stream.height);
        if !worth_resizing(now, wanted) {
            return;
        }
        // The new capture first: if it can't start, the old one goes on.
        match spawn_stream(
            &self.display,
            wanted,
            &self.settings,
            self.shows_cursor,
            self.codec,
            out,
        ) {
            Ok((stream, pump)) => {
                self.pump.abort();
                let old = std::mem::replace(&mut self.stream, stream);
                if let Err(e) = old.capture.stop() {
                    eprintln!("stopping capture: {e}");
                }
                self.pump = pump;
                println!(
                    "picture {}x{} → {}x{} (window {}x{})",
                    now.0, now.1, wanted.0, wanted.1, view.0, view.1
                );
            }
            Err(e) => eprintln!("can't capture at {}x{}: {e:#}", wanted.0, wanted.1),
        }
    }

    fn stop(self) {
        if let Some(s) = self.sound {
            s.stop();
        }
        self.pump.abort();
        if let Err(e) = self.stream.capture.stop() {
            eprintln!("stopping capture: {e}");
        }
        // Dropping the injector's sender releases held keys and buttons.
    }
}

async fn serve_daemon(path: &str) -> anyhow::Result<Ended> {
    let socket = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to the daemon at {path}"))?;
    println!("connected to the daemon");
    let (mut r, mut w) = socket.into_split();
    // Frames are bounded upstream (the stream drops when behind), and the
    // daemon reads as fast as it can, so this queue stays short.
    let (out, mut out_rx) = mpsc::unbounded_channel::<FromAgent>();
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ipc::write(&mut w, &msg).await.is_err() {
                break;
            }
        }
    });
    let _ = out.send(FromAgent::Hello {
        version: ipc::VERSION.into(),
    });

    let mut active: Option<Active> = None;
    let login_window = unsafe { libc::getuid() } == 0;
    let result = loop {
        let msg = tokio::select! {
            m = ipc::read::<ToAgent>(&mut r) => match m {
                Ok(Some(m)) => m,
                Ok(None) => break Ok(Ended::Closed),
                Err(e) => break Err(e),
            },
            // Seen at logout on macOS 27: the stream starts, then stops with
            // "Failed to find any displays or windows to capture".
            _ = CAPTURE_LOST.notified(), if login_window => {
                println!("capture lost in the login window");
                break Ok(Ended::Restart);
            }
        };
        match msg {
            ToAgent::Start {
                fps,
                mbps,
                in_flight,
                clipboard,
                codec,
                view,
                audio,
            } => {
                if let Some(a) = active.take() {
                    a.stop();
                }
                let settings = StreamSettings {
                    fps,
                    mbps,
                    in_flight: in_flight.max(1) as usize,
                };
                match start(&settings, &out, clipboard, codec, view, audio) {
                    Ok(a) => {
                        println!(
                            "capturing {}x{} for the daemon, {codec}",
                            a.stream.width, a.stream.height
                        );
                        active = Some(a);
                    }
                    Err(e) => {
                        eprintln!("can't capture: {e:#}");
                        let _ = out.send(FromAgent::Failed(format!("{e:#}")));
                        if login_window {
                            break Ok(Ended::Restart);
                        }
                    }
                }
            }
            ToAgent::Stop => {
                if let Some(a) = active.take() {
                    println!("stopped ({} input events injected)", a.inputs);
                    a.stop();
                }
            }
            ToAgent::ViewSize(width, height) => {
                if let Some(a) = &mut active {
                    a.view_size((width, height), &out);
                }
            }
            ToAgent::RequestKeyframe => {
                if let Some(a) = &active {
                    a.stream.keyframe.store(true, Ordering::Relaxed);
                }
            }
            ToAgent::Lock => {
                if let Some(a) = active.take() {
                    a.stop();
                }
                println!("locked the screen ({})", crate::lock::lock_screen());
            }
            ToAgent::Clipboard(text) => {
                if let Some(p) = active.as_ref().and_then(|a| a.pasteboard.as_ref()) {
                    p.set(text);
                }
            }
            ToAgent::Input(e) => {
                if let Some(a) = &mut active {
                    if a.inputs == 0 {
                        println!("first input event"); // not its content: could be a password key
                    }
                    a.inputs += 1;
                    let _ = a.injector.send(e);
                }
            }
        }
    };
    if let Some(a) = active.take() {
        a.stop();
    }
    // Let the daemon hear why, before a restart.
    drop(out);
    if tokio::time::timeout(Duration::from_secs(1), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
    result
}

fn start(
    settings: &StreamSettings,
    out: &mpsc::UnboundedSender<FromAgent>,
    clipboard: bool,
    codec: Codec,
    view: Option<(u32, u32)>,
    audio: bool,
) -> anyhow::Result<Active> {
    // Checked again here: the permission may have changed since we started.
    if !CGPreflightPostEventAccess() {
        println!("warning: no Accessibility permission now; input won't work");
    }
    let display = wait_for_display()?;
    // The pointer goes to the client separately where its shape can be read
    // (not at the login window); otherwise it stays in the video.
    let user_session = unsafe { libc::getuid() } != 0;
    let local_pointer = user_session && cursor::available();
    let size = picture_size((display.width as u32, display.height as u32), view);
    let (stream, pump) = spawn_stream(&display, size, settings, !local_pointer, codec, out)?;
    let _ = out.send(FromAgent::Started {
        width: display.width as u32,
        height: display.height as u32,
    });
    let last_move = cursor::LastMove::default();
    let pointer = if local_pointer {
        println!("pointer: drawn by the client");
        let out = out.clone();
        Some(cursor::Watcher::start(
            display.bounds,
            last_move.clone(),
            move |msg| {
                let _ = out.send(FromAgent::Control(msg));
            },
        ))
    } else {
        println!("pointer: in the video");
        let _ = out.send(FromAgent::Control(HostControl::CursorInVideo));
        None
    };
    // The login window has no user clipboard.
    let pasteboard = (clipboard && user_session).then(|| {
        let out = out.clone();
        pasteboard::Watcher::start(move |text| {
            let _ = out.send(FromAgent::Clipboard(text));
        })
    });
    // No sound (e.g. none at the login window) is no reason to fail.
    let sound = audio
        .then(|| {
            let out = out.clone();
            AudioCapture::start(
                &display,
                Box::new(move |p| {
                    let _ = out.send(FromAgent::Audio(p));
                }),
            )
            .inspect(|_| println!("sound: capturing"))
            .inspect_err(|e| eprintln!("no sound: {e}"))
            .ok()
        })
        .flatten();
    // The client plays it now; two speakers would echo.
    let muted = sound.as_ref().and_then(|_| crate::mute::mute());
    Ok(Active {
        sound,
        _muted: muted,
        injector: stream::spawn_injector(display.bounds, last_move),
        stream,
        pump,
        display,
        settings: *settings,
        shows_cursor: !local_pointer,
        codec,
        inputs: 0,
        pasteboard,
        _pointer: pointer,
    })
}

/// Starts capturing and encoding `display` at `size`, and a task that
/// passes the frames to the daemon.
fn spawn_stream(
    display: &capture::Display,
    size: (u32, u32),
    settings: &StreamSettings,
    shows_cursor: bool,
    codec: Codec,
    out: &mpsc::UnboundedSender<FromAgent>,
) -> anyhow::Result<(Stream, JoinHandle<()>)> {
    let mut stream = Stream::start(
        display,
        (size.0 as usize, size.1 as usize),
        settings,
        shows_cursor,
        codec,
    )?;
    let (_, mut frames) = tokio::sync::mpsc::channel(1);
    std::mem::swap(&mut frames, &mut stream.frames);
    let skipped = stream.skipped.clone();
    let out = out.clone();
    let pump = tokio::spawn(async move {
        let mut report = tokio::time::interval(Duration::from_secs(5));
        report.tick().await;
        loop {
            tokio::select! {
                item = frames.recv() => {
                    let msg = match item {
                        Some(Ok(f)) => FromAgent::Frame(f),
                        Some(Err(e)) => {
                            eprintln!("capture stopped: {e}");
                            // Queued first, so the daemon hears why.
                            let _ = out.send(FromAgent::Failed(e));
                            CAPTURE_LOST.notify_one();
                            break;
                        }
                        None => break,
                    };
                    if out.send(msg).is_err() {
                        break;
                    }
                }
                _ = report.tick() => {
                    let n = skipped.swap(0, Ordering::Relaxed);
                    if n > 0 {
                        println!("  {n} captures skipped in 5 s: encoder busy");
                    }
                }
            }
        }
    });
    Ok((stream, pump))
}

/// The main display. Right after the daemon wakes a sleeping display,
/// ScreenCaptureKit can still report none for a moment.
fn wait_for_display() -> anyhow::Result<capture::Display> {
    let deadline = std::time::Instant::now() + DISPLAY_WAIT;
    loop {
        match capture::main_display() {
            Ok(d) => return Ok(d),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(anyhow::anyhow!("{e} (waited {} s)", DISPLAY_WAIT.as_secs()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}
