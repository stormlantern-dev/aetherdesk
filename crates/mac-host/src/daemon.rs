//! `aether-agent daemon`: the root LaunchDaemon. The only network listener:
//! it checks the client's token and relays to the agent of the console
//! session, which captures and injects. It can't capture itself — a root
//! daemon has no window server session (M0).
//!
//! Agents connect to a unix socket and are told apart by the uid on the
//! socket (peer credentials, so they can't lie about it). The console
//! session is whoever owns `/dev/console`: root at the login window, the
//! user once logged in.

use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aetherdesk_proto::VideoFrame;
use aetherdesk_proto::quic;
use anyhow::{Context, anyhow, bail};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::ipc::{self, FromAgent, ToAgent};
use crate::serve;
use crate::session;
use crate::stream::StreamSettings;

pub const USAGE: &str = "\
usage: aether-agent daemon [--port N] [--token T] [--token-file PATH]
                           [--socket PATH] [--fps N] [--mbps N] [--in-flight N]

Runs as root (LaunchDaemon). Same options as serve, plus:

  --token-file PATH  where the token is when --token isn't given
                     (default /Library/Application Support/AetherDesk/token)
  --socket PATH      unix socket for the agents (default /var/run/aetherdesk/agent.sock)";

pub const TOKEN_FILE: &str = "/Library/Application Support/AetherDesk/token";

/// How long a client waits for an agent of the console session to show up,
/// e.g. right after login while launchd starts it.
const AGENT_WAIT: Duration = Duration::from_secs(10);

/// Frames queued per client before we drop and ask for a keyframe.
const FRAME_QUEUE: usize = 8;

pub fn run(args: &[String]) -> Result<(), String> {
    let mut token_file = TOKEN_FILE.to_string();
    let mut socket = ipc::SOCKET.to_string();
    let o = serve::parse(args, USAGE, |flag, val| {
        match flag {
            "--token-file" => token_file = val()?,
            "--socket" => socket = val()?,
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let token = match &o.token {
        Some(t) => t.clone(),
        None => read_token(&token_file)?,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime
        .block_on(daemon(&o, &token, &socket))
        .map_err(|e| format!("{e:#}"))
}

fn read_token(path: &str) -> Result<String, String> {
    let token = std::fs::read_to_string(path).map_err(|e| {
        format!("reading the token from {path}: {e} (scripts/mac.zsh install writes it)")
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("{path} is empty"));
    }
    Ok(token.to_string())
}

async fn daemon(o: &serve::Opts, token: &str, socket: &str) -> anyhow::Result<()> {
    let agents = Agents::default();
    let listener = bind_socket(socket)?;
    tokio::spawn(accept_agents(listener, agents.clone()));

    let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], o.port)))
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
    session::print_connect_hint(o.port, None);

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
        match client(&conn, token, &agents, s).await {
            Ok(()) => println!("{peer}: disconnected"),
            Err(e) => eprintln!("{peer}: {e:#}"),
        }
        conn.close(0u32.into(), b"bye");
    }
    Ok(())
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

/// Stops the agent's capture when the client session ends, however it ends.
struct InUse(Arc<Agent>);

impl Drop for InUse {
    fn drop(&mut self) {
        self.0.client.lock().unwrap().take();
        let _ = self.0.to.send(ToAgent::Stop);
    }
}

async fn client(
    conn: &quinn::Connection,
    token: &str,
    agents: &Agents,
    s: &StreamSettings,
) -> anyhow::Result<()> {
    let (mut ctl_send, ctl_recv) = session::handshake(conn, token).await?;
    let Some(agent) = agents.wait_for_console(AGENT_WAIT).await else {
        conn.close(3u32.into(), b"no agent for the console session");
        bail!(
            "no agent for the console session (uid {:?}) — is the agent installed?",
            console_uid()
        );
    };
    println!("  using agent {} (uid {})", agent.id, agent.uid);
    let (tx, mut rx) = mpsc::channel(FRAME_QUEUE);
    *agent.client.lock().unwrap() = Some(tx);
    let in_use = InUse(agent.clone());
    agent
        .to
        .send(ToAgent::Start {
            fps: s.fps,
            mbps: s.mbps,
            in_flight: s.in_flight as u32,
        })
        .map_err(|_| anyhow!("agent went away"))?;

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
    session::welcome(&mut ctl_send, width, height).await?;
    let mut video = conn.open_uni().await.context("video stream")?;

    // Agent messages → frames for the sender.
    let (frames_tx, mut frames) = mpsc::channel::<Result<VideoFrame, String>>(FRAME_QUEUE);
    let relay = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let item = match msg {
                FromAgent::Frame(f) => Ok(f),
                FromAgent::Failed(e) => Err(format!("agent: {e}")),
                _ => continue,
            };
            if frames_tx.send(item).await.is_err() {
                break;
            }
        }
    });
    let to = agent.to.clone();
    let control = tokio::spawn(session::read_control(ctl_send, ctl_recv, move || {
        let _ = to.send(ToAgent::RequestKeyframe);
    }));
    let to = agent.to.clone();
    let input = tokio::spawn(session::read_input(conn.clone(), move |e| {
        to.send(ToAgent::Input(e)).is_ok()
    }));
    let result = session::send_frames(conn, &mut video, &mut frames, None).await;
    if result.is_ok() && conn.close_reason().is_none() {
        println!("  agent {} went away", agent.id);
    }
    control.abort();
    input.abort();
    relay.abort();
    drop(in_use);
    result
}
