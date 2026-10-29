//! `aether-agent agent`: runs in a GUI session as a LaunchAgent — the login
//! window (as root) or a logged-in user's session. It connects to the
//! daemon and captures, encodes and injects when the daemon says so.
//! Screen Recording and Accessibility belong to this binary here, not to a
//! terminal.

use std::sync::atomic::Ordering;
use std::time::Duration;

use aetherdesk_proto::{HostControl, InputEvent};
use anyhow::Context;
use objc2_core_graphics::CGPreflightPostEventAccess;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime.block_on(async {
        let mut reported = false;
        loop {
            match serve_daemon(&socket).await {
                Ok(()) => println!("daemon closed the connection"),
                // Say it once, not every 2 s while the daemon is down.
                Err(e) if !reported => {
                    eprintln!("{e:#}; retrying every {} s", RETRY.as_secs());
                    reported = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(RETRY).await;
        }
    })
}

/// The capture the daemon asked for.
struct Active {
    stream: Stream,
    pump: JoinHandle<()>,
    injector: std::sync::mpsc::Sender<InputEvent>,
    /// Input events passed to the injector, for the log.
    inputs: u64,
    /// The session's clipboard, when the client asked for it.
    pasteboard: Option<pasteboard::Watcher>,
    /// Sends the pointer's shape, when the client draws it.
    _pointer: Option<cursor::Watcher>,
}

impl Active {
    fn stop(self) {
        self.pump.abort();
        if let Err(e) = self.stream.capture.stop() {
            eprintln!("stopping capture: {e}");
        }
        // Dropping the injector's sender releases held keys and buttons.
    }
}

async fn serve_daemon(path: &str) -> anyhow::Result<()> {
    let socket = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to the daemon at {path}"))?;
    println!("connected to the daemon");
    let (mut r, mut w) = socket.into_split();
    // Frames are bounded upstream (the stream drops when behind), and the
    // daemon reads as fast as it can, so this queue stays short.
    let (out, mut out_rx) = mpsc::unbounded_channel::<FromAgent>();
    let writer = tokio::spawn(async move {
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
    let result = loop {
        let msg = match ipc::read::<ToAgent>(&mut r).await {
            Ok(Some(m)) => m,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        match msg {
            ToAgent::Start {
                fps,
                mbps,
                in_flight,
                clipboard,
            } => {
                if let Some(a) = active.take() {
                    a.stop();
                }
                let settings = StreamSettings {
                    fps,
                    mbps,
                    in_flight: in_flight.max(1) as usize,
                };
                match start(&settings, &out, clipboard) {
                    Ok(a) => {
                        println!(
                            "capturing {}x{} for the daemon",
                            a.stream.width, a.stream.height
                        );
                        active = Some(a);
                    }
                    Err(e) => {
                        eprintln!("can't capture: {e:#}");
                        let _ = out.send(FromAgent::Failed(format!("{e:#}")));
                    }
                }
            }
            ToAgent::Stop => {
                if let Some(a) = active.take() {
                    println!("stopped ({} input events injected)", a.inputs);
                    a.stop();
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
    writer.abort();
    result
}

fn start(
    settings: &StreamSettings,
    out: &mpsc::UnboundedSender<FromAgent>,
    clipboard: bool,
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
    let mut stream = Stream::start(&display, settings, !local_pointer)?;
    let _ = out.send(FromAgent::Started {
        width: stream.width,
        height: stream.height,
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
                        Some(Err(e)) => FromAgent::Failed(e),
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
    Ok(Active {
        injector: stream::spawn_injector(display.bounds, last_move),
        stream,
        pump,
        inputs: 0,
        pasteboard,
        _pointer: pointer,
    })
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
