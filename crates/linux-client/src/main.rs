//! AetherDesk client for Linux (Wayland / Hyprland).

#[cfg(target_os = "linux")]
mod decoder;
#[cfg(target_os = "linux")]
mod discover;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod latency;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod pointer;
#[cfg(target_os = "linux")]
mod window;

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
mod testhooks;
#[cfg(target_os = "linux")]
mod trust;

#[cfg(target_os = "linux")]
const USAGE: &str = "\
usage: aetherdesk [HOST] [--no-clipboard] [--video FORMAT]
       aetherdesk pair [MAC] [--name NAME] [--code CODE]
       aetherdesk hosts
       aetherdesk unpair HOST

  HOST     a paired Mac, by name or address (default: $AETHERDESK_HOST, or the
           only paired Mac). Found on the LAN even when its address changed.
           Text copied on either side can be pasted on the other while
           connected; --no-clipboard turns that off both ways. --video asks
           for a video format: hevc444 (sharpest), hevc (~14 ms less delay)
           or h264. Default: the best the GPU decodes, else h264.
  pair     pair with a Mac: the only one found on the LAN, or MAC, its name
           as found on the LAN, an IP or a hostname (port defaults to 4500).
           First open pairing on the Mac: scripts/mac.zsh pair. It shows a
           code; type it here (or pass --code). --name stores the Mac under
           another name than its own.
  hosts    list the paired Macs, and other Macs found on the LAN
  unpair   forget a paired Mac";

#[cfg(target_os = "linux")]
enum Command {
    Connect {
        which: Option<String>,
        clipboard: bool,
        video: Option<aetherdesk_proto::Codec>,
    },
    Pair {
        address: Option<String>,
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
    let mut no_clipboard = false;
    let mut video = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = Some(it.next().ok_or("--name needs a value")?.clone()),
            "--code" => code = Some(it.next().ok_or("--code needs a value")?.clone()),
            "--no-clipboard" => no_clipboard = true,
            "--video" => {
                let name = it
                    .next()
                    .ok_or("--video needs a format: hevc444, hevc or h264")?;
                video = Some(decoder::parse_codec(name).ok_or(format!(
                    "unknown video format {name:?}: hevc444, hevc or h264"
                ))?);
            }
            "-h" | "--help" => return Err(USAGE.into()),
            "--version" => {
                println!(
                    "aetherdesk {} (protocol v{})",
                    env!("CARGO_PKG_VERSION"),
                    aetherdesk_proto::PROTOCOL_VERSION
                );
                let offered: Vec<String> =
                    decoder::codecs().iter().map(|c| c.to_string()).collect();
                let gpu = match decoder::gpu() {
                    Some(g) => format!("decoding on the GPU ({})", g.name()),
                    None => "decoding in software".into(),
                };
                println!("video: {} ({gpu})", offered.join(", "));
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
    let not_connecting = |cmd: &str| -> Result<(), String> {
        if no_clipboard || video.is_some() {
            return Err(format!(
                "--no-clipboard and --video are for connecting, not {cmd}"
            ));
        }
        Ok(())
    };
    match words.split_first() {
        Some((w, rest)) if w == "pair" => {
            not_connecting("pair")?;
            let (address, rest) = match rest.split_first() {
                Some((a, rest)) => (Some(a.clone()), rest),
                None => (None, rest),
            };
            extra(rest)?;
            Ok(Command::Pair {
                address,
                name,
                code,
            })
        }
        Some((w, rest)) if w == "hosts" => {
            no_flags("hosts")?;
            not_connecting("hosts")?;
            extra(rest)?;
            Ok(Command::Hosts)
        }
        Some((w, rest)) if w == "unpair" => {
            no_flags("unpair")?;
            not_connecting("unpair")?;
            let (host, rest) = rest.split_first().ok_or(USAGE)?;
            extra(rest)?;
            Ok(Command::Unpair(host.clone()))
        }
        Some((host, rest)) => {
            no_flags("connecting")?;
            extra(rest)?;
            Ok(Command::Connect {
                which: Some(host.clone()),
                clipboard: !no_clipboard,
                video,
            })
        }
        None => {
            no_flags("connecting")?;
            let env = std::env::var("AETHERDESK_HOST")
                .ok()
                .filter(|v| !v.is_empty());
            Ok(Command::Connect {
                which: env,
                clipboard: !no_clipboard,
                video,
            })
        }
    }
}

/// How long `pair` and `hosts` listen for Macs on the LAN.
#[cfg(target_os = "linux")]
const LAN_LOOKUP: std::time::Duration = std::time::Duration::from_secs(2);

/// Where to pair: an IP as given, a Mac found on the LAN by name, the only
/// Mac found on the LAN, or (no luck on the LAN) the name as a hostname.
#[cfg(target_os = "linux")]
fn pair_target(given: Option<String>) -> anyhow::Result<String> {
    use anyhow::anyhow;
    if let Some(a) = &given
        && (a.parse::<std::net::SocketAddr>().is_ok() || a.parse::<std::net::IpAddr>().is_ok())
    {
        return Ok(a.clone());
    }
    let finder = match discover::Finder::start() {
        Ok(f) => f,
        Err(e) => {
            return given
                .ok_or_else(|| anyhow!("{e}; give the Mac's address: aetherdesk pair ADDRESS"));
        }
    };
    let found = match &given {
        Some(name) => finder.collect(LAN_LOOKUP, |all| {
            all.iter().any(|f| f.name.eq_ignore_ascii_case(name))
        }),
        None => {
            eprintln!("looking for Macs on the LAN…");
            finder.collect(LAN_LOOKUP, |_| false)
        }
    };
    let pick = |f: &discover::Found| {
        println!("found {} on the LAN at {}", f.name, f.address);
        f.address.to_string()
    };
    match given {
        Some(name) => Ok(found
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(&name))
            .map(pick)
            .unwrap_or(name)),
        None => match found.as_slice() {
            [] => Err(anyhow!(
                "no Mac found on the LAN. Is the AetherDesk host installed and \
                 running there? Otherwise give its address: aetherdesk pair ADDRESS"
            )),
            [one] => Ok(pick(one)),
            several => Err(anyhow!(
                "found several Macs on the LAN; say which: {}",
                several
                    .iter()
                    .map(|f| format!("{:?}", f.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

#[cfg(target_os = "linux")]
fn execute(command: Command) -> anyhow::Result<()> {
    use anyhow::anyhow;

    let store = trust::Store::user().map_err(|e| anyhow!(e))?;
    match command {
        Command::Connect {
            which,
            clipboard,
            video,
        } => {
            if let Some(v) = video {
                decoder::prefer(v);
            }
            let host = store.find(which.as_deref()).map_err(|e| anyhow!(e))?;
            let id = store.identity().map_err(|e| anyhow!(e))?;
            run(host, id, clipboard)
        }
        Command::Pair {
            address,
            name,
            code,
        } => {
            let id = store.identity().map_err(|e| anyhow!(e))?;
            // Find the Mac before asking for the code: the code only lasts
            // two minutes.
            let address = pair_target(address)?;
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
            let found = match discover::Finder::start() {
                Ok(f) => f.collect(LAN_LOOKUP, |_| false),
                Err(e) => {
                    eprintln!("not looking on the LAN: {e}");
                    Vec::new()
                }
            };
            if hosts.is_empty() {
                println!("not paired with any host");
            }
            for h in &hosts {
                let lan = match found.iter().find(|f| f.fingerprint == h.fingerprint) {
                    Some(f) => format!(", on the LAN at {}", f.address),
                    None => String::new(),
                };
                println!(
                    "{}  {}  ({}{lan})",
                    h.fingerprint.short(),
                    h.name,
                    h.address
                );
            }
            let others: Vec<_> = found
                .iter()
                .filter(|f| hosts.iter().all(|h| h.fingerprint != f.fingerprint))
                .collect();
            if !others.is_empty() {
                println!("\nnot paired, on the LAN:");
                for f in others {
                    println!("{}  {}  ({})", f.fingerprint.short(), f.name, f.address);
                }
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
fn run(
    host: trust::Host,
    id: aetherdesk_proto::trust::Identity,
    clipboard: bool,
) -> anyhow::Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    println!("connecting to {} ({})…", host.name, host.address);
    let title = host.name.clone();
    let link = net::connect(host, id, clipboard)?;
    let welcome = link.welcome.clone();
    println!(
        "connected: remote display {}x{} px, {}",
        welcome.width, welcome.height, welcome.codec
    );
    let codec = welcome.codec;

    // Decoder thread: every frame must be decoded (P-frames build on each
    // other), but only the newest picture needs to reach the screen.
    let (pictures_tx, pictures) = mpsc::sync_channel(2);
    let keyframes = link.keyframe_requests.clone();
    let input = link.input.clone();
    let frames = link.frames;
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let mut decoder = match decoder::Decoder::new(codec) {
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
            clipboard: link.clipboard,
            pointer: link.pointer,
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
