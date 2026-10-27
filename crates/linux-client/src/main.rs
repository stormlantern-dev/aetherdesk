//! AetherDesk client for Linux (Wayland / Hyprland).

#[cfg(target_os = "linux")]
fn main() {
    println!(
        "aetherdesk {} (protocol v{}) — not implemented yet",
        env!("CARGO_PKG_VERSION"),
        aetherdesk_proto::PROTOCOL_VERSION
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the aetherdesk client only runs on Linux");
    std::process::exit(1);
}
