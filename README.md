# AetherDesk

A modern, secure, high-performance remote desktop for **Wayland (Hyprland)** and **macOS**.

> **Status:** early planning — no code yet. See the [Product Requirements Document](docs/PRD.md).

## Goals

- **Secure session lifecycle** — spawn and tear down sessions from a system daemon (PAM on Linux, native session management on macOS) instead of requiring a logged-in, unlocked desktop.
- **GPU-native streaming** — zero-copy capture and hardware encoding (DMA-BUF + PipeWire + VA-API/NVENC on Linux, ScreenCaptureKit + VideoToolbox on macOS), AV1 first, HEVC as fallback, with correct color-space handling.
- **Transparent keyboard** — explicit keyboard grab/release so Cmd+Tab, Spotlight, `$mainMod` and friends reach the remote side, plus a configurable host escape key.

## Planned stack

- Rust
- QUIC / WebRTC (DataChannels + RTP)
- AV1 / HEVC

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
