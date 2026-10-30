//! `aether-agent daemon`: the root LaunchDaemon. The only network listener:
//! it admits paired clients (by certificate) and relays to the agent of the console
//! session, which captures and injects. It can't capture itself — a root
//! daemon has no window server session (M0).
//!
//! Agents connect to a unix socket and are told apart by the uid on the
//! socket (peer credentials, so they can't lie about it). The console
//! session is whoever owns `/dev/console`: root at the login window, the
//! user once logged in. A client stays connected across login and logout;
//! the daemon switches it to the new session's agent.

use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aetherdesk_proto::CLOSE_NO_AGENT;
use aetherdesk_proto::trust::Identity;
use aetherdesk_proto::{Codec, HostControl, SessionKind, VideoFrame};
use aetherdesk_proto::{discovery, quic};
use anyhow::{Context, anyhow, bail};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::bonjour;
use crate::ipc::{self, FromAgent, ToAgent};
use crate::power;
use crate::serve;
use crate::session::{self, Admitted};
use crate::stream::StreamSettings;
use crate::trust::Store;

pub const USAGE: &str = "\
usage: aether-agent daemon [--port N] [--socket PATH] [--fps N] [--mbps N] [--in-flight N]

Runs as root (LaunchDaemon). Same options as serve, plus:

  --socket PATH  unix socket for the agents (default /var/run/aetherdesk/agent.sock)

Its identity and paired clients are in /Library/Application Support/AetherDesk;
pair a client with `sudo aether-agent pair` (scripts/mac.zsh pair).";

/// How long a client waits for an agent of the console session to show up,
/// e.g. right after login while launchd starts it.
const AGENT_WAIT: Duration = Duration::from_secs(10);

/// Frames queued per client before we drop and ask for a keyframe.
const FRAME_QUEUE: usize = 8;

/// How often a session checks who owns the console.
const CONSOLE_POLL: Duration = Duration::from_millis(500);

/// After a client's link died (Wi-Fi, sleep), how long it may take to come
/// back before the screen locks. A client that leaves on purpose locks it
/// right away.
const LOCK_GRACE: Duration = Duration::from_secs(30);

/// How long a session survives without an agent, after login or logout,
/// while launchd starts the next session's agent. At logout the login
/// window's agent may fail and restart (launchd waits 10 s between
/// launches), so room for two fresh agents.
const SWITCH_WAIT: Duration = Duration::from_secs(35);

pub fn run(args: &[String]) -> Result<(), String> {
    let mut socket = ipc::SOCKET.to_string();
    let o = serve::parse(args, USAGE, |flag, val| {
        match flag {
            "--socket" => socket = val()?,
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let store = Store::system();
    let id = store
        .identity()
        .map_err(|e| format!("identity in {}: {e}", store.dir().display()))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime
        .block_on(daemon(&o, &store, &id, &socket))
        .map_err(|e| format!("{e:#}"))
}

async fn daemon(o: &serve::Opts, store: &Store, id: &Identity, socket: &str) -> anyhow::Result<()> {
    let agents = Agents::default();
    let listener = bind_socket(socket)?;
    tokio::spawn(accept_agents(listener, agents.clone()));

    let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], o.port)), id)
        .map_err(|e| anyhow!("listening on UDP port {}: {e}", o.port))?;
    let s = &o.stream;
    println!(
        "daemon {}: UDP port {}, agents on {socket} (up to {} fps, {} Mbit/s, {} in flight)",
        ipc::VERSION,
        o.port,
        s.fps,
        s.mbps,
        s.in_flight
    );
    session::print_connect_hint(o.port, store, "scripts/mac.zsh pair");
    let host = id.fingerprint();
    // Kept for the daemon's lifetime: dropping it withdraws the announcement.
    let _announced = match bonjour::announce(o.port, &host) {
        Ok(a) => {
            println!(
                "announced on the LAN as {}: `aetherdesk pair` on Linux finds this Mac",
                discovery::SERVICE
            );
            Some(a)
        }
        Err(e) => {
            eprintln!("not announced on the LAN ({e}); clients need the address");
            None
        }
    };

    // One client at a time: others wait in the accept queue.
    let accept = async {
        // A lock that waits for a client that lost its link: it may be back
        // within LOCK_GRACE.
        let mut pending_lock: Option<tokio::time::Instant> = None;
        loop {
            let incoming = tokio::select! {
                i = endpoint.accept() => match i {
                    Some(i) => i,
                    None => break,
                },
                () = sleep_until(pending_lock), if pending_lock.is_some() => {
                    pending_lock = None;
                    lock_console(&agents, "the client didn't come back");
                    continue;
                }
            };
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
                    if pending_lock.take().is_some() {
                        println!("  the client is back: not locking");
                    }
                    match client(&conn, send, recv, &agents, s, clipboard, codec, audio).await {
                        Ok(()) => println!("{peer}: disconnected"),
                        Err(e) => eprintln!("{peer}: {e:#}"),
                    }
                    if link_lost(&conn) {
                        println!(
                            "  link lost; locking the screen in {} s unless the client is back",
                            LOCK_GRACE.as_secs()
                        );
                        pending_lock = Some(tokio::time::Instant::now() + LOCK_GRACE);
                    } else {
                        lock_console(&agents, "the client left");
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

/// Did the session end because the link died (the client may come back),
/// rather than the client leaving or the session failing here?
fn link_lost(conn: &quinn::Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(
            quinn::ConnectionError::TimedOut
                | quinn::ConnectionError::Reset
                | quinn::ConnectionError::TransportError(_)
        )
    )
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Locks the screen of the logged-in user on the console, so a session
/// that ended doesn't leave the desktop open. Nothing to do at the login
/// window.
fn lock_console(agents: &Agents, why: &str) {
    match console_uid() {
        Some(0) | None => {}
        Some(uid) => match agents.for_console() {
            Some(agent) => {
                println!(
                    "  {why}: locking the screen (agent {}, uid {uid})",
                    agent.id
                );
                let _ = agent.to.send(ToAgent::Lock);
            }
            None => eprintln!("  {why}, but there's no agent for uid {uid} to lock the screen"),
        },
    }
}

/// The agents' socket. Anyone may connect; the uid on the connection decides
/// which session an agent stands for.
fn bind_socket(path: &str) -> anyhow::Result<UnixListener> {
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let _ = std::fs::remove_file(path); // left over from a previous run
    let listener = UnixListener::bind(path).with_context(|| format!("binding {path}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("chmod {path}"))?;
    Ok(listener)
}

/// One connected agent.
struct Agent {
    id: u64,
    uid: u32,
    pid: i32,
    to: mpsc::UnboundedSender<ToAgent>,
    /// The client session using this agent, if any: where its messages go.
    client: Mutex<Option<mpsc::Sender<FromAgent>>>,
    /// The client's connection, when it wants sound: packets go straight
    /// there, never queued behind video.
    sound: Mutex<Option<quinn::Connection>>,
}

#[derive(Default, Clone)]
struct Agents(Arc<Mutex<Vec<Arc<Agent>>>>);

impl Agents {
    /// The newest agent of the console session's user.
    fn for_console(&self) -> Option<Arc<Agent>> {
        let uid = console_uid()?;
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|a| a.uid == uid)
            .cloned()
    }

    fn contains(&self, id: u64) -> bool {
        self.0.lock().unwrap().iter().any(|a| a.id == id)
    }

    async fn wait_for_console(&self, timeout: Duration) -> Option<Arc<Agent>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(agent) = self.for_console() {
                return Some(agent);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Owner of the console: 0 at the login window, the user once logged in.
fn console_uid() -> Option<u32> {
    std::fs::metadata("/dev/console").ok().map(|m| m.uid())
}

async fn accept_agents(listener: UnixListener, agents: Agents) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("agent socket: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let agents = agents.clone();
        tokio::spawn(async move {
            if let Err(e) = agent_connection(id, stream, &agents).await {
                eprintln!("agent {id}: {e:#}");
            }
        });
    }
}

async fn agent_connection(id: u64, stream: UnixStream, agents: &Agents) -> anyhow::Result<()> {
    let cred = stream.peer_cred().context("peer credentials")?;
    let (mut r, mut w) = stream.into_split();
    let hello = tokio::time::timeout(Duration::from_secs(5), ipc::read::<FromAgent>(&mut r))
        .await
        .context("no hello within 5 s")??;
    match hello {
        Some(FromAgent::Hello { version }) if version == ipc::VERSION => {}
        Some(FromAgent::Hello { version }) => {
            bail!(
                "agent is version {version}, daemon {}; reinstall both",
                ipc::VERSION
            )
        }
        other => bail!("expected hello, got {other:?}"),
    }
    let (to, mut to_rx) = mpsc::unbounded_channel::<ToAgent>();
    let agent = Arc::new(Agent {
        id,
        uid: cred.uid(),
        pid: cred.pid().unwrap_or(-1),
        to,
        client: Mutex::new(None),
        sound: Mutex::new(None),
    });
    println!("agent {id}: uid {}, pid {}", agent.uid, agent.pid);
    agents.0.lock().unwrap().push(agent.clone());

    let writer = tokio::spawn(async move {
        while let Some(msg) = to_rx.recv().await {
            if ipc::write(&mut w, &msg).await.is_err() {
                break;
            }
        }
    });
    let result = loop {
        let msg = match ipc::read::<FromAgent>(&mut r).await {
            Ok(Some(m)) => m,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        if let FromAgent::Audio(packet) = &msg {
            if let Some(conn) = agent.sound.lock().unwrap().as_ref() {
                session::send_audio(conn, packet);
            }
            continue;
        }
        let client = agent.client.lock().unwrap().clone();
        let Some(client) = client else {
            continue; // nobody watching, e.g. frames still flushing after Stop
        };
        if let FromAgent::Frame(_) = msg {
            // The client is behind: drop, and resync with a keyframe.
            if let Err(TrySendError::Full(_)) = client.try_send(msg) {
                let _ = agent.to.send(ToAgent::RequestKeyframe);
            }
        } else {
            let _ = client.send(msg).await;
        }
    };
    agents.0.lock().unwrap().retain(|a| a.id != id);
    // Dropping the client's sender ends its session.
    agent.client.lock().unwrap().take();
    writer.abort();
    println!("agent {id}: gone");
    result
}

/// Stops the agent's capture when the client stops using it: at a session
/// switch or when the client leaves, however it leaves.
/// The client's window size in pixels, once it said.
type View = Arc<Mutex<Option<(u32, u32)>>>;

struct InUse(Arc<Agent>);

impl Drop for InUse {
    fn drop(&mut self) {
        self.0.client.lock().unwrap().take();
        self.0.sound.lock().unwrap().take();
        let _ = self.0.to.send(ToAgent::Stop);
    }
}

/// Points `agent`'s messages at this client and starts its capture.
fn attach(
    agent: &Arc<Agent>,
    tx: &mpsc::Sender<FromAgent>,
    s: &StreamSettings,
    clipboard: bool,
    codec: Codec,
    view: &View,
    sound: Option<&quinn::Connection>,
) -> anyhow::Result<InUse> {
    *agent.client.lock().unwrap() = Some(tx.clone());
    *agent.sound.lock().unwrap() = sound.cloned();
    let in_use = InUse(agent.clone());
    agent
        .to
        .send(ToAgent::Start {
            fps: s.fps,
            mbps: s.mbps,
            in_flight: s.in_flight as u32,
            clipboard,
            codec,
            view: *view.lock().unwrap(),
            audio: sound.is_some(),
        })
        .map_err(|_| anyhow!("agent {} went away", agent.id))?;
    Ok(in_use)
}

fn kind(agent: &Agent) -> SessionKind {
    if agent.uid == 0 {
        SessionKind::LoginWindow
    } else {
        SessionKind::User
    }
}

#[expect(clippy::too_many_arguments)]
async fn client(
    conn: &quinn::Connection,
    mut ctl_send: quinn::SendStream,
    ctl_recv: quinn::RecvStream,
    agents: &Agents,
    s: &StreamSettings,
    clipboard: bool,
    codec: Codec,
    audio: bool,
) -> anyhow::Result<()> {
    // Before picking the agent: a sleeping display has to come back before
    // the agent can capture it.
    let _awake = power::keep_awake();
    let Some(agent) = agents.wait_for_console(AGENT_WAIT).await else {
        conn.close(CLOSE_NO_AGENT.into(), b"no agent for the console session");
        bail!(
            "no agent for the console session (uid {:?}) — is the agent installed?",
            console_uid()
        );
    };
    println!("  using agent {} (uid {})", agent.id, agent.uid);
    // Every agent this session uses sends here, one after the other.
    let (tx, mut rx) = mpsc::channel(FRAME_QUEUE);
    // The client's window size, as last said; a new agent starts at it.
    let view: View = Arc::default();
    let sound = audio.then(|| conn.clone());
    let in_use = attach(&agent, &tx, s, clipboard, codec, &view, sound.as_ref())?;

    let (width, height) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(FromAgent::Started { width, height }) => return Ok((width, height)),
                Some(FromAgent::Failed(e)) => bail!("agent: {e}"),
                Some(_) => continue,
                None => bail!("agent went away"),
            }
        }
    })
    .await
    .context("agent didn't start capturing within 10 s")??;
    session::welcome(&mut ctl_send, width, height, codec).await?;
    let mut video = conn.open_uni().await.context("video stream")?;

    // Keyframe requests and input go to whichever agent is current.
    let current = Arc::new(Mutex::new(agent.clone()));
    let (notices, notices_rx) = mpsc::unbounded_channel();
    let _ = notices.send(HostControl::Session(kind(&agent)));
    let to = current.clone();
    let control = tokio::spawn(session::read_control(
        ctl_send,
        ctl_recv,
        notices_rx,
        {
            let to = to.clone();
            move || {
                let _ = to.lock().unwrap().to.send(ToAgent::RequestKeyframe);
            }
        },
        // At the login window there's no clipboard; that agent ignores it.
        {
            let to = to.clone();
            move |text| {
                let _ = to.lock().unwrap().to.send(ToAgent::Clipboard(text));
            }
        },
        {
            let view = view.clone();
            move |width, height| {
                *view.lock().unwrap() = Some((width, height));
                let _ = to.lock().unwrap().to.send(ToAgent::ViewSize(width, height));
            }
        },
    ));
    let to = current.clone();
    let input = tokio::spawn(session::read_input(conn.clone(), move |e| {
        // At the login window macOS ignores these; sent anyway, harmless.
        let _ = to.lock().unwrap().to.send(ToAgent::Input(e));
        true
    }));

    let (frames_tx, mut frames) = mpsc::channel::<Result<VideoFrame, String>>(FRAME_QUEUE);
    let follow = tokio::spawn(follow_console(Following {
        rx,
        tx,
        in_use,
        current,
        agents: agents.clone(),
        notices,
        settings: *s,
        clipboard,
        codec,
        view,
        frames: frames_tx,
        sound,
    }));
    let result = session::send_frames(conn, &mut video, &mut frames, None).await;
    control.abort();
    input.abort();
    // Dropping the follower drops its InUse, which stops the capture.
    follow.abort();
    result
}

/// What [`follow_console`] works with.
struct Following {
    rx: mpsc::Receiver<FromAgent>,
    tx: mpsc::Sender<FromAgent>,
    in_use: InUse,
    current: Arc<Mutex<Arc<Agent>>>,
    agents: Agents,
    notices: mpsc::UnboundedSender<HostControl>,
    settings: StreamSettings,
    clipboard: bool,
    codec: Codec,
    view: View,
    frames: mpsc::Sender<Result<VideoFrame, String>>,
    /// The client's connection, when it wants sound.
    sound: Option<quinn::Connection>,
}

/// Relays the current agent's frames, and switches to the agent of the new
/// console session at login and logout. The new agent starts with a
/// keyframe, so the client just keeps decoding; a different size is fine,
/// the client follows the stream. Ends the session only when no working
/// agent for the console shows up within [`SWITCH_WAIT`].
///
/// A failed capture doesn't end the session right away: at logout the
/// user's capture fails moments before its agent is killed and the login
/// window's agent takes over.
async fn follow_console(mut f: Following) {
    let mut poll = tokio::time::interval(CONSOLE_POLL);
    let mut orphaned: Option<tokio::time::Instant> = None;
    // Why the current agent stopped delivering, if it did.
    let mut failed: Option<String> = None;
    loop {
        tokio::select! {
            msg = f.rx.recv() => {
                let item = match msg {
                    Some(FromAgent::Frame(frame)) => Ok(frame),
                    Some(FromAgent::Failed(e)) => {
                        println!("  agent {} failed: {e}; waiting for the next session", f.in_use.0.id);
                        failed = Some(e);
                        continue;
                    }
                    Some(FromAgent::Started { width, height }) => {
                        println!("  agent {} capturing {width}x{height}", f.in_use.0.id);
                        continue;
                    }
                    // Sent straight to the client by the agent's reader.
                    Some(FromAgent::Hello { .. } | FromAgent::Audio(_)) => continue,
                    Some(FromAgent::Clipboard(text)) => {
                        let _ = f.notices.send(HostControl::Clipboard(text));
                        continue;
                    }
                    Some(FromAgent::Control(msg)) => {
                        let _ = f.notices.send(msg);
                        continue;
                    }
                    None => return, // can't happen while we hold tx
                };
                if f.frames.send(item).await.is_err() {
                    return;
                }
            }
            _ = poll.tick() => {
                let old = f.in_use.0.clone();
                match f.agents.for_console() {
                    Some(next) if next.id != old.id => {
                        println!(
                            "  console changed: agent {} (uid {}) → agent {} (uid {})",
                            old.id, old.uid, next.id, next.uid
                        );
                        match attach(
                            &next,
                            &f.tx,
                            &f.settings,
                            f.clipboard,
                            f.codec,
                            &f.view,
                            f.sound.as_ref(),
                        ) {
                            Ok(in_use) => {
                                f.in_use = in_use; // drops the old one: Stop
                                *f.current.lock().unwrap() = next.clone();
                                let _ = f.notices.send(HostControl::Session(kind(&next)));
                                orphaned = None;
                                failed = None;
                            }
                            Err(e) => eprintln!("  {e:#}"),
                        }
                    }
                    _ if failed.is_some() || !f.agents.contains(old.id) => {
                        // Mid-switch: the old agent failed or is gone, the
                        // new one not there yet. The client keeps the last
                        // picture.
                        let since = *orphaned.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() > SWITCH_WAIT {
                            let why = match &failed {
                                Some(e) => format!("agent {}: {e}", old.id),
                                None => format!("agent {} went away", old.id),
                            };
                            let _ = f
                                .frames
                                .send(Err(format!(
                                    "{why}, and no agent for the console session (uid {:?}) \
                                     took over within {} s",
                                    console_uid(),
                                    SWITCH_WAIT.as_secs()
                                )))
                                .await;
                            return;
                        }
                    }
                    _ => orphaned = None,
                }
            }
        }
    }
}
