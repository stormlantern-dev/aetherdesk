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
/// while launchd starts the next session's agent. Counted again for every
/// agent that takes over, so it also covers the login window's agent
/// restarting (launchd waits 10 s between launches).
const SWITCH_WAIT: Duration = Duration::from_secs(20);

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
    let agents = Agents::system();
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
                    if link_lost(conn.close_reason()) {
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
fn link_lost(reason: Option<quinn::ConnectionError>) -> bool {
    matches!(
        reason,
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
    match agents.console() {
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

/// Who owns the console: [`console_uid`] in the daemon, a stand-in in tests.
type ConsoleOwner = Arc<dyn Fn() -> Option<u32> + Send + Sync>;

/// The connected agents, and how to tell which one has the console.
#[derive(Clone)]
struct Agents {
    list: Arc<Mutex<Vec<Arc<Agent>>>>,
    console: ConsoleOwner,
}

impl Agents {
    fn system() -> Self {
        Self {
            list: Arc::default(),
            console: Arc::new(console_uid),
        }
    }

    /// The console's owner: 0 at the login window, the user once logged in.
    fn console(&self) -> Option<u32> {
        (self.console)()
    }

    /// The newest agent of the console session's user.
    fn for_console(&self) -> Option<Arc<Agent>> {
        let uid = self.console()?;
        self.list
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|a| a.uid == uid)
            .cloned()
    }

    fn contains(&self, id: u64) -> bool {
        self.list.lock().unwrap().iter().any(|a| a.id == id)
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
    agents.list.lock().unwrap().push(agent.clone());

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
    agents.list.lock().unwrap().retain(|a| a.id != id);
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
            agents.console()
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
                                    f.agents.console(),
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

/// The session-switching scenarios from the manual tests (#18, #29, #71),
/// with stand-in agents and a console owner the test sets. Time is paused,
/// so the 20 s waits take no time.
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use aetherdesk_proto::HostTiming;

    use super::*;

    /// Fails a test that waits for something that never comes, instead of
    /// hanging it. Instant while time is paused.
    async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(60), f)
            .await
            .expect("waited 60 s for something that didn't come")
    }

    const USER: u32 = 501;
    const LOGIN: u32 = 0;
    const NOBODY: u32 = u32::MAX;

    /// Agents whose console owner the test sets.
    fn agents(console: u32) -> (Agents, Arc<AtomicU32>) {
        let owner = Arc::new(AtomicU32::new(console));
        let o = owner.clone();
        let agents = Agents {
            list: Arc::default(),
            console: Arc::new(move || {
                let uid = o.load(Ordering::Relaxed);
                (uid != NOBODY).then_some(uid)
            }),
        };
        (agents, owner)
    }

    /// An agent that connected, as `agent_connection` registers it.
    fn add(agents: &Agents, id: u64, uid: u32) -> (Arc<Agent>, mpsc::UnboundedReceiver<ToAgent>) {
        let (to, rx) = mpsc::unbounded_channel();
        let agent = Arc::new(Agent {
            id,
            uid,
            pid: 1000 + id as i32,
            to,
            client: Mutex::new(None),
            sound: Mutex::new(None),
        });
        agents.list.lock().unwrap().push(agent.clone());
        (agent, rx)
    }

    /// The agent's process ended, as `agent_connection` cleans up.
    fn gone(agents: &Agents, agent: &Agent) {
        agents.list.lock().unwrap().retain(|a| a.id != agent.id);
        agent.client.lock().unwrap().take();
    }

    /// The agent sends `msg` to whichever client uses it.
    async fn emit(agent: &Agent, msg: FromAgent) {
        let client = agent.client.lock().unwrap().clone();
        client.expect("no client attached").send(msg).await.unwrap();
    }

    fn frame(pts_us: u64) -> FromAgent {
        FromAgent::Frame(VideoFrame {
            pts_us,
            keyframe: true,
            timing: HostTiming::default(),
            data: vec![0, 0, 0, 1],
        })
    }

    const SETTINGS: StreamSettings = StreamSettings {
        fps: 60,
        mbps: 20,
        in_flight: 1,
    };

    /// A client session as `client` sets it up, minus the network.
    struct Session {
        frames: mpsc::Receiver<Result<VideoFrame, String>>,
        notices: mpsc::UnboundedReceiver<HostControl>,
        current: Arc<Mutex<Arc<Agent>>>,
        view: View,
        task: tokio::task::JoinHandle<()>,
    }

    fn connect(agents: &Agents) -> Session {
        let agent = agents.for_console().expect("no agent for the console");
        let (tx, rx) = mpsc::channel(FRAME_QUEUE);
        let view: View = Arc::new(Mutex::new(Some((800, 600))));
        let in_use = attach(&agent, &tx, &SETTINGS, true, Codec::Hevc444, &view, None).unwrap();
        let current = Arc::new(Mutex::new(agent));
        let (notices, notices_rx) = mpsc::unbounded_channel();
        let (frames_tx, frames) = mpsc::channel(FRAME_QUEUE);
        let task = tokio::spawn(follow_console(Following {
            rx,
            tx,
            in_use,
            current: current.clone(),
            agents: agents.clone(),
            notices,
            settings: SETTINGS,
            clipboard: true,
            codec: Codec::Hevc444,
            view: view.clone(),
            frames: frames_tx,
            sound: None,
        }));
        Session {
            frames,
            notices: notices_rx,
            current,
            view,
            task,
        }
    }

    impl Session {
        async fn next_frame(&mut self) -> u64 {
            match within(self.frames.recv()).await {
                Some(Ok(f)) => f.pts_us,
                other => panic!("expected a frame, got {other:?}"),
            }
        }

        fn current(&self) -> u64 {
            self.current.lock().unwrap().id
        }
    }

    /// The `Start` an agent gets; returns the view size it carries.
    async fn started(rx: &mut mpsc::UnboundedReceiver<ToAgent>) -> Option<(u32, u32)> {
        match within(rx.recv()).await {
            Some(ToAgent::Start {
                view,
                codec,
                clipboard,
                audio,
                ..
            }) => {
                assert_eq!(codec, Codec::Hevc444);
                assert!(clipboard);
                assert!(!audio);
                view
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn logout_hands_the_session_to_the_login_window() {
        let (agents, console) = agents(USER);
        let (user, mut user_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        assert_eq!(started(&mut user_rx).await, Some((800, 600)));
        emit(&user, frame(1)).await;
        assert_eq!(s.next_frame().await, 1);

        // The client's window changed size meanwhile: the next agent starts there.
        *s.view.lock().unwrap() = Some((1024, 768));
        // Logout: the user's capture fails, then the login window's agent shows up.
        emit(&user, FromAgent::Failed("capture stopped".into())).await;
        console.store(LOGIN, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (login, mut login_rx) = add(&agents, 2, LOGIN);

        assert_eq!(started(&mut login_rx).await, Some((1024, 768)));
        assert_eq!(within(user_rx.recv()).await, Some(ToAgent::Stop));
        assert_eq!(
            within(s.notices.recv()).await,
            Some(HostControl::Session(SessionKind::LoginWindow))
        );
        assert_eq!(s.current(), 2);
        emit(&login, frame(2)).await;
        assert_eq!(s.next_frame().await, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn login_hands_the_session_to_the_user() {
        let (agents, console) = agents(LOGIN);
        let (login, mut login_rx) = add(&agents, 1, LOGIN);
        let mut s = connect(&agents);
        started(&mut login_rx).await;
        emit(&login, frame(1)).await;
        assert_eq!(s.next_frame().await, 1);

        // Login: the user's agent shows up while the login window's still runs.
        console.store(USER, Ordering::Relaxed);
        let (user, mut user_rx) = add(&agents, 2, USER);
        started(&mut user_rx).await;
        assert_eq!(within(login_rx.recv()).await, Some(ToAgent::Stop));
        assert_eq!(
            within(s.notices.recv()).await,
            Some(HostControl::Session(SessionKind::User))
        );
        // Frames the old agent still flushes no longer reach the client.
        assert!(login.client.lock().unwrap().is_none());
        emit(&user, frame(2)).await;
        assert_eq!(s.next_frame().await, 2);
        assert_eq!(s.current(), 2);
    }

    /// #71: the login window's agent can't capture, exits, and launchd
    /// starts a fresh one 10 s later.
    #[tokio::test(start_paused = true)]
    async fn a_restarted_login_window_agent_takes_over() {
        let (agents, console) = agents(USER);
        let (user, mut user_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        started(&mut user_rx).await;

        emit(&user, FromAgent::Failed("capture stopped".into())).await;
        console.store(LOGIN, Ordering::Relaxed);
        gone(&agents, &user);
        let (first, mut first_rx) = add(&agents, 2, LOGIN);
        started(&mut first_rx).await;
        emit(
            &first,
            FromAgent::Failed("Start stream failed (code 1003)".into()),
        )
        .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        gone(&agents, &first);

        tokio::time::sleep(Duration::from_secs(10)).await;
        let (fresh, mut fresh_rx) = add(&agents, 3, LOGIN);
        started(&mut fresh_rx).await;
        emit(&fresh, frame(7)).await;
        assert_eq!(s.next_frame().await, 7);
        assert_eq!(s.current(), 3);
    }

    /// The wait starts again with every agent that takes over: restarts at
    /// launchd's 10 s throttle keep the session, however many.
    #[tokio::test(start_paused = true)]
    async fn every_restart_gets_the_full_wait() {
        let (agents, _console) = agents(LOGIN);
        let (first, mut first_rx) = add(&agents, 1, LOGIN);
        let mut s = connect(&agents);
        started(&mut first_rx).await;
        emit(&first, FromAgent::Failed("code 1003".into())).await;
        gone(&agents, &first);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let (second, mut second_rx) = add(&agents, 2, LOGIN);
        started(&mut second_rx).await;
        emit(&second, FromAgent::Failed("code 1003".into())).await;
        gone(&agents, &second);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let (third, mut third_rx) = add(&agents, 3, LOGIN);
        started(&mut third_rx).await;
        emit(&third, FromAgent::Failed("code 1003".into())).await;
        gone(&agents, &third);
        // 30 s without a picture by now, more than SWITCH_WAIT in total.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let (fourth, mut fourth_rx) = add(&agents, 4, LOGIN);
        started(&mut fourth_rx).await;
        emit(&fourth, frame(4)).await;
        assert_eq!(s.next_frame().await, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_session_ends_when_no_agent_takes_over() {
        let (agents, console) = agents(USER);
        let (user, mut user_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        started(&mut user_rx).await;
        let t0 = tokio::time::Instant::now();
        emit(&user, FromAgent::Failed("capture stopped".into())).await;
        console.store(LOGIN, Ordering::Relaxed);

        let err = match within(s.frames.recv()).await {
            Some(Err(e)) => e,
            other => panic!("expected the session to end, got {other:?}"),
        };
        assert!(err.contains("agent 1: capture stopped"), "{err}");
        assert!(
            err.contains(&format!("within {} s", SWITCH_WAIT.as_secs())),
            "{err}"
        );
        let waited = t0.elapsed();
        assert!(waited >= SWITCH_WAIT && waited < SWITCH_WAIT + Duration::from_secs(2));
        within(s.task).await.unwrap();
    }

    /// #18: kill the agent while connected; launchd restarts it.
    #[tokio::test(start_paused = true)]
    async fn a_killed_agent_is_replaced_by_its_restart() {
        let (agents, _console) = agents(USER);
        let (old, mut old_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        started(&mut old_rx).await;
        gone(&agents, &old);
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (new, mut new_rx) = add(&agents, 2, USER);
        started(&mut new_rx).await;
        assert_eq!(
            within(s.notices.recv()).await,
            Some(HostControl::Session(SessionKind::User))
        );
        emit(&new, frame(5)).await;
        assert_eq!(s.next_frame().await, 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gone_agent_ends_the_session_after_the_wait() {
        let (agents, _console) = agents(USER);
        let (old, mut old_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        started(&mut old_rx).await;
        gone(&agents, &old);
        let err = match within(s.frames.recv()).await {
            Some(Err(e)) => e,
            other => panic!("expected the session to end, got {other:?}"),
        };
        assert!(err.contains("agent 1 went away"), "{err}");
    }

    /// A session that just carries on: no switch, no notices.
    #[tokio::test(start_paused = true)]
    async fn a_steady_session_stays_put() {
        let (agents, _console) = agents(USER);
        let (user, mut user_rx) = add(&agents, 1, USER);
        let mut s = connect(&agents);
        started(&mut user_rx).await;
        for i in 0..5 {
            emit(&user, frame(i)).await;
            assert_eq!(s.next_frame().await, i);
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        assert!(s.notices.try_recv().is_err());
        assert!(user_rx.try_recv().is_err());
        assert_eq!(s.current(), 1);
    }

    #[test]
    fn the_newest_agent_of_the_console_user_is_used() {
        let (agents, console) = agents(USER);
        assert!(agents.for_console().is_none());
        let _login = add(&agents, 1, LOGIN);
        let _old = add(&agents, 2, USER);
        let _new = add(&agents, 3, USER);
        assert_eq!(agents.for_console().unwrap().id, 3);
        console.store(LOGIN, Ordering::Relaxed);
        assert_eq!(agents.for_console().unwrap().id, 1);
        console.store(NOBODY, Ordering::Relaxed);
        assert!(agents.for_console().is_none());
    }

    /// #13: leaving locks the Mac; at the login window there's nothing to lock.
    #[test]
    fn leaving_locks_the_users_screen_only() {
        let (agents, console) = agents(USER);
        let (_login, mut login_rx) = add(&agents, 1, LOGIN);
        let (_user, mut user_rx) = add(&agents, 2, USER);
        lock_console(&agents, "the client left");
        assert_eq!(user_rx.try_recv(), Ok(ToAgent::Lock));
        console.store(LOGIN, Ordering::Relaxed);
        lock_console(&agents, "the client left");
        assert!(login_rx.try_recv().is_err());
        assert!(user_rx.try_recv().is_err());
    }

    /// A dead link waits LOCK_GRACE before locking; leaving locks at once.
    #[test]
    fn only_a_dead_link_counts_as_lost() {
        use quinn::ConnectionError as E;
        assert!(link_lost(Some(E::TimedOut)));
        assert!(link_lost(Some(E::Reset)));
        assert!(!link_lost(Some(E::LocallyClosed)));
        assert!(!link_lost(None));
    }

    /// An agent over the real socket protocol: version check, registration
    /// by peer uid, and a keyframe request when the client falls behind.
    #[tokio::test]
    async fn agent_connection_registers_and_resyncs_a_slow_client() {
        let (agents, _console) = agents(NOBODY);
        let (daemon_end, agent_end) = UnixStream::pair().unwrap();
        let task = tokio::spawn({
            let agents = agents.clone();
            async move { agent_connection(7, daemon_end, &agents).await }
        });
        let (mut r, mut w) = agent_end.into_split();
        ipc::write(
            &mut w,
            &FromAgent::Hello {
                version: ipc::VERSION.into(),
            },
        )
        .await
        .unwrap();
        let agent = loop {
            if let Some(a) = agents.list.lock().unwrap().first().cloned() {
                break a;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(agent.id, 7);
        assert_eq!(agent.uid, unsafe { libc::getuid() });

        // A client with room for one frame.
        let (tx, mut rx) = mpsc::channel(1);
        *agent.client.lock().unwrap() = Some(tx);
        ipc::write(&mut w, &frame(1)).await.unwrap();
        ipc::write(&mut w, &frame(2)).await.unwrap();
        assert_eq!(
            within(ipc::read::<ToAgent>(&mut r)).await.unwrap(),
            Some(ToAgent::RequestKeyframe)
        );
        assert!(matches!(within(rx.recv()).await, Some(FromAgent::Frame(f)) if f.pts_us == 1));

        // The agent's process ends: it's forgotten, and its client hears so.
        drop(w);
        drop(r);
        within(task).await.unwrap().unwrap();
        assert!(agents.list.lock().unwrap().is_empty());
        assert!(within(rx.recv()).await.is_none());
    }

    #[tokio::test]
    async fn an_agent_of_another_version_is_refused() {
        let (agents, _console) = agents(NOBODY);
        let (daemon_end, agent_end) = UnixStream::pair().unwrap();
        let (_r, mut w) = agent_end.into_split();
        ipc::write(
            &mut w,
            &FromAgent::Hello {
                version: "0.0.0+ipc0".into(),
            },
        )
        .await
        .unwrap();
        let err = agent_connection(1, daemon_end, &agents).await.unwrap_err();
        assert!(format!("{err:#}").contains("reinstall both"), "{err:#}");
        assert!(agents.list.lock().unwrap().is_empty());
    }
}
