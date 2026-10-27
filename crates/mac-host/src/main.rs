//! AetherDesk host agent for macOS. For now it runs by hand in a logged-in
//! session and listens directly (M1); the root daemon comes in M2.

#[cfg(target_os = "macos")]
fn main() {
    println!(
        "aether-agent {} (protocol v{}) — not implemented yet",
        env!("CARGO_PKG_VERSION"),
        aetherdesk_proto::PROTOCOL_VERSION
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("aether-agent only runs on macOS");
    std::process::exit(1);
}
