//! AetherDesk client for Linux (Wayland / Hyprland).

#[cfg(target_os = "linux")]
mod decoder;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod window;

#[cfg(target_os = "linux")]
const USAGE: &str = "\
usage: aetherdesk <host[:port]> --token T

  host     the Mac's IP address or hostname; port defaults to 4500
  --token  the token `aether-agent serve` printed";

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result =
        parse(&args).and_then(|(host, token)| run(&host, &token).map_err(|e| format!("{e:#}")));
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn parse(args: &[String]) -> Result<(String, String), String> {
    let (mut host, mut token) = (None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--token" => token = Some(it.next().ok_or("--token needs a value")?.clone()),
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
            a if host.is_none() => host = Some(a.to_string()),
            a => return Err(format!("unexpected argument {a}\n\n{USAGE}")),
        }
    }
    match (host, token) {
        (Some(h), Some(t)) => Ok((h, t)),
        _ => Err(USAGE.into()),
    }
}

#[cfg(target_os = "linux")]
fn run(host: &str, token: &str) -> anyhow::Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    println!("connecting to {host}…");
    let session = net::connect(host, token)?;
    let welcome = session.welcome.clone();
    println!(
        "connected: remote display {}x{} px",
        welcome.width, welcome.height
    );

    // Decoder thread: every frame must be decoded (P-frames build on each
    // other), but only the newest picture needs to reach the screen.
    let (pictures_tx, pictures) = mpsc::sync_channel(2);
    let keyframes = session.keyframe_requests.clone();
    let input = session.input.clone();
    let frames = session.frames;
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
            for frame in frames {
                let result = decoder.decode(&frame, |p| {
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

    window::run(&welcome, host, pictures, input)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the aetherdesk client only runs on Linux");
    std::process::exit(1);
}
