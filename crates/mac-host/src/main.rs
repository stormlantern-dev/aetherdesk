//! AetherDesk host for macOS, one binary in three roles:
//!
//! - `daemon`: root LaunchDaemon, the network side; relays to an agent.
//! - `agent`: LaunchAgent per GUI session (login window, user); captures,
//!   encodes and injects.
//! - `serve`: both in one process, for running by hand from a terminal.
//! - `menu`: the menu-bar item with the updater; what AetherDesk.app does
//!   when it's opened without arguments.

#[cfg(target_os = "macos")]
mod admin;
#[cfg(target_os = "macos")]
mod agent;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod annexb;
#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod bonjour;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod bundle;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cursor;
#[cfg(target_os = "macos")]
mod daemon;
#[cfg(target_os = "macos")]
mod dump;
#[cfg(target_os = "macos")]
mod encoder;
#[cfg(target_os = "macos")]
mod indicator;
#[cfg(target_os = "macos")]
mod inject;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod input;
#[cfg(target_os = "macos")]
mod install;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod ipc;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod keymap;
#[cfg(target_os = "macos")]
mod lock;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod mute;
#[cfg(target_os = "macos")]
mod output;
#[cfg(target_os = "macos")]
mod pasteboard;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod pipeline;
#[cfg(target_os = "macos")]
mod power;
#[cfg(target_os = "macos")]
mod record;
#[cfg(target_os = "macos")]
mod scale;
#[cfg(target_os = "macos")]
mod serve;
#[cfg(target_os = "macos")]
mod session;
#[cfg(target_os = "macos")]
mod speaker;
#[cfg(target_os = "macos")]
mod stream;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod trust;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod tuning;

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        // Opened as an app (Finder, `open`, Sparkle's relaunch). Old macOS
        // versions passed a -psn_ argument.
        None if in_bundle() => menu::run(&[]),
        Some(a) if a.starts_with("-psn_") && in_bundle() => menu::run(&[]),
        Some("menu") => menu::run(&args[1..]),
        Some("record") => record::run(&args[1..]),
        Some("serve") => serve::run(&args[1..]),
        Some("daemon") => daemon::run(&args[1..]),
        Some("agent") => agent::run(&args[1..]),
        Some("pair") => admin::pair(&args[1..]),
        Some("clients") => admin::list(&args[1..]),
        Some("unpair") => admin::unpair(&args[1..]),
        Some("doctor") => doctor(),
        Some("install") => install::install(&args[1..]),
        Some("uninstall") => install::uninstall(&args[1..]),
        Some("status") => install::status(&args[1..]),
        Some("restart") => install::restart(&args[1..]),
        Some("_install-root") => install::install_root(&args[1..]),
        Some("_uninstall-root") => install::uninstall_root(&args[1..]),
        Some("_postinstall") => install::postinstall(&args[1..]),
        Some("_info-plist") => info_plist(&args[1..]),
        Some("_indicator") => indicator::run(&args[1..]),
        Some("version") | Some("--version") => {
            println!(
                "aether-agent {} (protocol v{})",
                env!("CARGO_PKG_VERSION"),
                aetherdesk_proto::PROTOCOL_VERSION
            );
            Ok(())
        }
        _ => Err(format!(
            "usage: aether-agent install|uninstall|status|restart|pair|clients|unpair|doctor|daemon|agent|serve|record|menu|version\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}",
            install::USAGE,
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

/// Whether this binary was opened as an app: from inside AetherDesk.app,
/// and not from a terminal (there, no arguments shows the usage).
#[cfg(target_os = "macos")]
fn in_bundle() -> bool {
    let terminal = unsafe { libc::isatty(0) } == 1;
    !terminal
        && std::env::current_exe()
            .ok()
            .and_then(|p| bundle::enclosing_bundle(&p))
            .is_some()
}

/// `aether-agent _info-plist [--ed-key <public key>] [--feed <url>]`: the
/// app's Info.plist, for packaging/macos/app-bundle.sh.
#[cfg(target_os = "macos")]
fn info_plist(args: &[String]) -> Result<(), String> {
    let usage = "usage: aether-agent _info-plist [--ed-key <public key>] [--feed <url>]";
    let (mut key, mut feed) = (None, bundle::FEED_URL.to_string());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ed-key" => key = Some(it.next().ok_or(usage)?.clone()),
            "--feed" => feed = it.next().ok_or(usage)?.clone(),
            _ => return Err(usage.into()),
        }
    }
    print!(
        "{}",
        bundle::info_plist(env!("CARGO_PKG_VERSION"), &feed, key.as_deref())?
    );
    Ok(())
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
