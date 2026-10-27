//! AetherDesk host agent for macOS. For now it runs by hand in a logged-in
//! session (M1); the root daemon comes in M2.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod annexb;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod inject;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod input;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod keymap;
#[cfg(target_os = "macos")]
mod record;
#[cfg(target_os = "macos")]
mod serve;

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("record") => record::run(&args[1..]),
        Some("serve") => serve::run(&args[1..]),
        Some("version") | Some("--version") => {
            println!(
                "aether-agent {} (protocol v{})",
                env!("CARGO_PKG_VERSION"),
                aetherdesk_proto::PROTOCOL_VERSION
            );
            Ok(())
        }
        _ => Err(format!(
            "usage: aether-agent serve|record|version\n\n{}\n\n{}",
            serve::USAGE,
            record::USAGE
        )),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("aether-agent only runs on macOS");
    std::process::exit(1);
}
