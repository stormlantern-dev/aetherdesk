//! AetherDesk client for Linux (Wayland / Hyprland).

#[cfg(target_os = "linux")]
mod decoder;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod latency;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod window;

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
mod testhooks;
#[cfg(target_os = "linux")]
mod trust;

#[cfg(target_os = "linux")]
const USAGE: &str = "\
usage: aetherdesk [HOST]
       aetherdesk pair ADDRESS [--name NAME] [--code CODE]
       aetherdesk hosts
       aetherdesk unpair HOST

  HOST     a paired Mac, by name or address (default: $AETHERDESK_HOST, or the
           only paired Mac)
  pair     pair with the Mac at ADDRESS (IP or hostname, port defaults to 4500).
           First open pairing on the Mac: scripts/mac.zsh pair. It shows a
           code; type it here (or pass --code). --name stores the Mac under
           another name than its own.
  hosts    list the paired Macs
  unpair   forget a paired Mac";

#[cfg(target_os = "linux")]
enum Command {
    Connect(Option<String>),
    Pair {
        address: String,
        name: Option<String>,
        code: Option<String>,
    },
    Hosts,
    Unpair(String),
}

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = parse(&args).and_then(|c| execute(c).map_err(|e| format!("{e:#}")));
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn parse(args: &[String]) -> Result<Command, String> {
    let mut words = Vec::new();
    let (mut name, mut code) = (None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = Some(it.next().ok_or("--name needs a value")?.clone()),
            "--code" => code = Some(it.next().ok_or("--code needs a value")?.clone()),
            "-h" | "--help" => return Err(USAGE.into()),
            "--version" => {
                println!(
                    "aetherdesk {} (protocol v{})",
                    env!("CARGO_PKG_VERSION"),
                    aetherdesk_proto::PROTOCOL_VERSION
                );
                std::process::exit(0);
            }
            a if a.starts_with('-') => return Err(format!("unknown option {a}\n\n{USAGE}")),
            a => words.push(a.to_string()),
        }
    }
    let extra = |w: &[String]| -> Result<(), String> {
        match w.first() {
            Some(a) => Err(format!("unexpected argument {a}\n\n{USAGE}")),
            None => Ok(()),
        }
    };
    let no_flags = |cmd: &str| -> Result<(), String> {
        if name.is_some() || code.is_some() {
            return Err(format!("--name and --code are for pair, not {cmd}"));
        }
        Ok(())
    };
    match words.split_first() {
        Some((w, rest)) if w == "pair" => {
            let (address, rest) = rest.split_first().ok_or(USAGE)?;
            extra(rest)?;
            Ok(Command::Pair {
                address: address.clone(),
                name,
                code,
            })
        }
        Some((w, rest)) if w == "hosts" => {
            no_flags("hosts")?;
            extra(rest)?;
            Ok(Command::Hosts)
        }
        Some((w, rest)) if w == "unpair" => {
            no_flags("unpair")?;
            let (host, rest) = rest.split_first().ok_or(USAGE)?;
            extra(rest)?;
            Ok(Command::Unpair(host.clone()))
        }
        Some((host, rest)) => {
            no_flags("connecting")?;
            extra(rest)?;
            Ok(Command::Connect(Some(host.clone())))
        }
        None => {
            no_flags("connecting")?;
            let env = std::env::var("AETHERDESK_HOST")
                .ok()
                .filter(|v| !v.is_empty());
            Ok(Command::Connect(env))
        }
    }
}

#[cfg(target_os = "linux")]
fn execute(command: Command) -> anyhow::Result<()> {
    use anyhow::anyhow;

    let store = trust::Store::user().map_err(|e| anyhow!(e))?;
    match command {
        Command::Connect(which) => {
            let host = store.find(which.as_deref()).map_err(|e| anyhow!(e))?;
            let id = store.identity().map_err(|e| anyhow!(e))?;
            run(host, id)
        }
        Command::Pair {
            address,
            name,
            code,
        } => {
            let id = store.identity().map_err(|e| anyhow!(e))?;
            let code = match code {
                Some(c) => c,
                None => {
                    eprint!("Code shown on the Mac: ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line
                }
            };
            let code = aetherdesk_proto::trust::normalize_code(&code);
            if code.len() != 6 {
                return Err(anyhow!("the code has 6 digits"));
            }
            let mut host = net::pair(&address, &code, &trust::client_name(), &id)?;
            if let Some(n) = name {
                host.name = n;
            }
            store.add(host.clone()).map_err(|e| anyhow!(e))?;
            println!(
                "Paired with {} ({}). Connect with: aetherdesk {:?}",
                host.name,
                host.fingerprint.short(),
                host.name
            );
            Ok(())
        }
        Command::Hosts => {
            let hosts = store.hosts().map_err(|e| anyhow!(e))?;
            if hosts.is_empty() {
                println!("not paired with any host");
            }
            for h in hosts {
                println!("{}  {}  ({})", h.fingerprint.short(), h.name, h.address);
            }
            Ok(())
        }
        Command::Unpair(which) => {
            let gone = store.remove(&which).map_err(|e| anyhow!(e))?;
            if gone.is_empty() {
                return Err(anyhow!(
                    "not paired with {which:?}; `aetherdesk hosts` lists them"
                ));
            }
            for h in gone {
                println!(
                    "forgot {} ({}). The Mac still lists this client until you unpair it there.",
                    h.name,
                    h.fingerprint.short()
                );
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn run(host: trust::Host, id: aetherdesk_proto::trust::Identity) -> anyhow::Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    println!("connecting to {} ({})…", host.name, host.address);
    let title = host.name.clone();
    let link = net::connect(host, id)?;
    let welcome = link.welcome.clone();
    println!(
        "connected: remote display {}x{} px",
        welcome.width, welcome.height
    );

    // Decoder thread: every frame must be decoded (P-frames build on each
    // other), but only the newest picture needs to reach the screen.
    let (pictures_tx, pictures) = mpsc::sync_channel(2);
    let keyframes = link.keyframe_requests.clone();
    let input = link.input.clone();
    let frames = link.frames;
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let mut decoder = match decoder::Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("decoder: {e:#}");
                    return;
                }
            };
            let mut last_request = None::<Instant>;
            for (frame, mut timing) in frames {
                timing.decode_start = latency::now_us();
                let result = decoder.decode(&frame, timing, |p| {
                    let _ = pictures_tx.try_send(p);
                });
                if let Err(e) = result {
                    // Lost sync; ask for a keyframe, at most twice a second.
                    if last_request.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                        eprintln!("decode error ({e}), requesting keyframe");
                        let _ = keyframes.send(());
                        last_request = Some(Instant::now());
                    }
                }
            }
        })?;

    let shown = window::run(
        &welcome,
        &title,
        pictures,
        input,
        window::LinkView {
            sync: &link.sync,
            login_window: &link.login_window,
            state: &link.state,
            last_heard: &link.last_heard,
            retry: link.retry.clone(),
        },
    );
    // Say goodbye, so the host knows we left on purpose (and locks the Mac
    // now) instead of waiting to see whether the link comes back.
    link.closer.close();
    shown?;
    if let Some(reason) = link.ended.lock().unwrap().take()
        && !reason.is_empty()
    {
        eprintln!("{reason}");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the aetherdesk client only runs on Linux");
    std::process::exit(1);
}
