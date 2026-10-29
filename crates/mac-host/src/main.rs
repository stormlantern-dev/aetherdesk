//! AetherDesk host for macOS, one binary in three roles:
//!
//! - `daemon`: root LaunchDaemon, the network side; relays to an agent.
//! - `agent`: LaunchAgent per GUI session (login window, user); captures,
//!   encodes and injects.
//! - `serve`: both in one process, for running by hand from a terminal.

#[cfg(target_os = "macos")]
mod admin;
#[cfg(target_os = "macos")]
mod agent;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod annexb;
#[cfg(target_os = "macos")]
mod bonjour;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cursor;
#[cfg(target_os = "macos")]
mod daemon;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod inject;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod input;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod ipc;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod keymap;
#[cfg(target_os = "macos")]
mod lock;
#[cfg(target_os = "macos")]
mod pasteboard;
#[cfg(target_os = "macos")]
mod power;
#[cfg(target_os = "macos")]
mod record;
#[cfg(target_os = "macos")]
mod serve;
#[cfg(target_os = "macos")]
mod session;
#[cfg(target_os = "macos")]
mod stream;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod trust;

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("record") => record::run(&args[1..]),
        Some("serve") => serve::run(&args[1..]),
        Some("daemon") => daemon::run(&args[1..]),
        Some("agent") => agent::run(&args[1..]),
        Some("pair") => admin::pair(&args[1..]),
        Some("clients") => admin::list(&args[1..]),
        Some("unpair") => admin::unpair(&args[1..]),
        Some("doctor") => doctor(),
        Some("version") | Some("--version") => {
            println!(
                "aether-agent {} (protocol v{})",
                env!("CARGO_PKG_VERSION"),
                aetherdesk_proto::PROTOCOL_VERSION
            );
            Ok(())
        }
        _ => Err(format!(
            "usage: aether-agent daemon|agent|serve|pair|clients|unpair|record|doctor|version\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}",
            daemon::USAGE,
            admin::USAGE,
            agent::USAGE,
            serve::USAGE,
            record::USAGE
        )),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Checks what the agent needs on this Mac and prints it, so problems are
/// easy to spot (and to paste into a bug report).
#[cfg(target_os = "macos")]
fn doctor() -> Result<(), String> {
    use objc2_core_graphics::{CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess};

    println!(
        "aether-agent {} (protocol v{})",
        env!("CARGO_PKG_VERSION"),
        aetherdesk_proto::PROTOCOL_VERSION
    );
    let screen = CGPreflightScreenCaptureAccess();
    let input = CGPreflightPostEventAccess();
    let status = |ok: bool| if ok { "ok" } else { "MISSING" };
    println!("screen recording permission: {}", status(screen));
    println!("accessibility permission:    {}", status(input));
    if !screen || !input {
        println!(
            "  → grant it to the app this runs from (your terminal) in System Settings → \
             Privacy & Security, then restart that app"
        );
    }
    match capture::main_display() {
        Ok(d) => println!(
            "main display: {}x{} px ({}x{} pt)",
            d.width, d.height, d.bounds.width, d.bounds.height
        ),
        Err(e) => println!("main display: {e}"),
    }
    let addresses = session::local_addresses();
    println!(
        "addresses (UDP port {} for serve):",
        aetherdesk_proto::DEFAULT_PORT
    );
    for (interface, ip) in &addresses {
        println!("  {ip}  ({interface})");
    }
    if addresses.is_empty() {
        println!("  none — is the Mac on a network?");
    }
    if screen && input {
        Ok(())
    } else {
        Err("missing permissions".into())
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("aether-agent only runs on macOS");
    std::process::exit(1);
}
