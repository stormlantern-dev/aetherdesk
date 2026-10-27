//! M0 spike: can we capture and type at the macOS lock screen / login window?
//!
//! `m0 probe` takes a ScreenCaptureKit screenshot every second and saves it as a
//! PNG, logs session / permission / Secure Event Input state, and after a delay
//! types a harmless lowercase string (never Return) via `CGEventPost`.
//! Throwaway code — see issue #3 and docs/MVP-PLAN.md (M0).

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = mac::run(std::env::args().skip(1).collect()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("m0 only runs on macOS");
    std::process::exit(1);
}
