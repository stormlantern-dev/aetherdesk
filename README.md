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

## Installing the Linux client

On Arch Linux and CachyOS, build and install the client with the PKGBUILD in [`packaging/arch/`](packaging/arch/). No Rust setup needed beyond what `makepkg` pulls in, and `pacman` tracks the files:

```sh
git clone https://github.com/stormlantern-dev/aetherdesk
cd aetherdesk/packaging/arch/aetherdesk-git   # latest main; or ../aetherdesk for the tagged release
makepkg -si
```

This installs `aetherdesk`, a launcher entry with icon, and the licences. `sudo pacman -R aetherdesk-git` (or `aetherdesk`) removes them again; your pairings in `~/.config/aetherdesk/` stay.

Tagged releases also come with a prebuilt tarball on the [releases page](https://github.com/stormlantern-dev/aetherdesk/releases); its `INSTALL.md` lists the dependencies (SDL3, ffmpeg).

## Development

Cargo workspace under [`crates/`](crates/):

- `proto` — wire protocol shared by client and host
- `mac-host` — the macOS host agent (`aether-agent`)
- `linux-client` — the Wayland/Hyprland client (`aetherdesk`)

[`spikes/`](spikes/) holds throwaway experiments, outside the workspace.

The **Linux side** (Hyprland client and shared crates) is developed in a devcontainer — see [`.devcontainer/`](.devcontainer/). It's Arch-based to match a CachyOS host and passes through the host's Wayland socket and NVIDIA GPU, so the client window opens straight on Hyprland.

Host requirements: Podman, the `devcontainer` CLI, and `nvidia-container-toolkit` with a CDI spec (`nvidia-ctk cdi list` should show `nvidia.com/gpu=all`). Without an NVIDIA GPU, drop that `--device` line from `devcontainer.json`.

```nu
devcontainer up --docker-path podman --workspace-folder .
devcontainer exec --docker-path podman --workspace-folder . bash
```

The **macOS host** is built natively on the Mac. A container can't help there: Docker on macOS runs a Linux VM, without ScreenCaptureKit, VideoToolbox or code signing. The devcontainer can still *type-check* it: `cargo clippy -p aetherdesk-mac-host --target aarch64-apple-darwin`.

### Running it

On the Mac, either install the host, so it runs at the login window, on the lock screen and in your session:

```zsh
scripts/mac.zsh install    # asks for sudo; then grant the installed binary Screen Recording + Accessibility
scripts/mac.zsh status
```

Then pair your Linux machine, once. On the Mac:

```zsh
scripts/mac.zsh pair       # shows a 6-digit code for 2 minutes
```

On Linux:

```bash
aetherdesk pair <mac-ip>   # asks for the code
aetherdesk                 # connects; with several paired Macs: aetherdesk <name>
```

Pairing stores each side's certificate on the other; after that only paired machines get in, and the client only talks to the Mac it paired with. `aetherdesk hosts` and `scripts/mac.zsh clients` list them; `unpair` on either side forgets one.

Or run the host by hand in a logged-in session (your terminal needs Screen Recording permission). It keeps its own pairings, so pair with `aether-agent pair --serve`:

```zsh
cargo run --release -p aetherdesk-mac-host -- serve
```

Without a Mac, `cargo run --example fake_host` streams a test pattern on port 4500 (pairing code `123456`); [`scripts/e2e-fake-host.sh`](scripts/e2e-fake-host.sh) pairs the client with it and runs it, as CI does.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
