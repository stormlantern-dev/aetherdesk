# AetherDesk

A modern, secure, high-performance remote desktop for **Wayland (Hyprland)** and **macOS**.

> **Status:** alpha. Use a Mac from Linux: a macOS host (Apple Silicon) streams its screen to a Linux client (Wayland, tested on Hyprland). See [Install](#install-alpha) to try it; the plan is in [docs/MVP-PLAN.md](docs/MVP-PLAN.md).

## Goals

- **Secure session lifecycle** — spawn and tear down sessions from a system daemon (PAM on Linux, native session management on macOS) instead of requiring a logged-in, unlocked desktop.
- **GPU-native streaming** — zero-copy capture and hardware encoding (DMA-BUF + PipeWire + VA-API/NVENC on Linux, ScreenCaptureKit + VideoToolbox on macOS), AV1 first, HEVC as fallback, with correct color-space handling.
- **Transparent keyboard** — explicit keyboard grab/release so Cmd+Tab, Spotlight, `$mainMod` and friends reach the remote side, plus a configurable host key (`--host-key`).

## Planned stack

- Rust
- QUIC / WebRTC (DataChannels + RTP)
- AV1 / HEVC

## Install (alpha)

You need a **Mac** with Apple Silicon and macOS 14 or later (the host), and a **Linux** desktop on Wayland (the client). Both install from the [Homebrew tap](https://github.com/stormlantern-dev/homebrew-tap).

### 1. On the Mac

```sh
brew install --cask stormlantern-dev/tap/aetherdesk-host
aether-agent install
```

`aether-agent install` asks for your password: it puts the host in `/Library/Application Support/AetherDesk` and starts it, now and at every login (also at the login window, view-only there).

Then, **once**, open System Settings → Privacy & Security and add `/Library/Application Support/AetherDesk/aether-agent` to both **Screen & System Audio Recording** and **Accessibility** (the + button, then Cmd+Shift+G and paste the path). `aether-agent status` shows whether it runs.

### 2. On Linux

```sh
brew install --cask stormlantern-dev/tap/aetherdesk
```

The alpha build is linked against current Arch Linux libraries, so for now it runs on an up-to-date **Arch or CachyOS** (`pacman -S sdl3 ffmpeg`). Elsewhere, build it from source (see [Development](#development)). An NVIDIA (RTX 20 and later) or recent Intel GPU decodes on the GPU and gets the sharpest picture (HEVC 4:4:4); otherwise it decodes H.264 in software.

### 3. Pair, once

On the Mac:

```sh
sudo aether-agent pair
```

shows a 6-digit code for 2 minutes. On Linux, on the same network:

```sh
aetherdesk pair
```

finds the Mac and asks for the code. (Not on the same network? `aetherdesk pair <mac-address>`.)

### 4. Connect

```sh
aetherdesk
```

Keyboard and mouse go to the Mac while the window has them; **Right Ctrl** gives them back to Linux and grabs them again (`--host-key` picks another key, see [Keyboard and the host key](#keyboard-and-the-host-key)). Copy and paste work both ways. When you close the window the Mac locks.

Away from home, connect to the Mac's VPN or Tailscale address: `aetherdesk <address>`. The client only accepts the Mac it paired with, whatever the address.

### Updating and removing

- **Update:** `brew upgrade`, then on the Mac `aether-agent install` again (the installed copy is what runs). Releases are signed with a Developer ID and notarized, so the two permissions stay granted across updates.
- **Remove:** on the Mac `aether-agent uninstall`, then `brew uninstall --cask aetherdesk-host`; on Linux `brew uninstall --cask aetherdesk`.

### If it doesn't connect

- `aether-agent status` on the Mac: are the daemon and agent running? Missing permissions show in its log.
- A VPN on a narrow network (some mobile and guest networks): since 0.1.0-alpha.1 that works, but if a connection times out where SSH works, tell us your VPN's MTU.
- The client keeps a log of its last session in `~/.local/state/aetherdesk/client.log`.

### Building the Linux client with the PKGBUILD

On Arch Linux and CachyOS you can also build and install the client with the PKGBUILD in [`packaging/arch/`](packaging/arch/); `pacman` then tracks the files:

```sh
git clone https://github.com/stormlantern-dev/aetherdesk
cd aetherdesk/packaging/arch/aetherdesk-git   # latest main; or ../aetherdesk for the tagged release
makepkg -si
```

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
aetherdesk pair            # finds the Mac on the LAN and asks for the code
aetherdesk                 # connects; with several paired Macs: aetherdesk <name>
```

The installed host announces itself on the LAN (Bonjour, `_aetherdesk._udp`), so `aetherdesk pair` finds it, and `aetherdesk` still finds it after its address changed. With several Macs on the LAN, `aetherdesk pair "<its name>"`; over Tailscale or a VPN, `aetherdesk pair <address>`. What's on the LAN is only a hint: the client still only accepts the certificate it paired with.

The Mac sends pictures at the size of your window (in pixels), not at its own full resolution: fewer pixels to capture, encode, send and decode. Resize the window and the picture follows a moment later.

### Keyboard and the host key

While the window has the keyboard ("grabbed"), every key goes to the Mac, including the ones your compositor would take, such as `$mainMod` (Super) combinations on Hyprland: Cmd+Tab, Spotlight and the rest work on the Mac. **Right Ctrl** gives keyboard and mouse back to Linux, and pressing it again (or clicking the picture) grabs them again; the badge in the window says which side has them. The host key itself never reaches the Mac.

If Right Ctrl is taken (a compose key, a `$mainMod` variant), pick another key: `aetherdesk --host-key ScrollLock`, or set `$AETHERDESK_HOST_KEY` (the option wins). Any SDL key name works, such as `RightAlt`, `Pause` or `F13`; keys needed for typing (letters, digits, Enter, Esc, Tab, Space, the left modifiers, right Shift) are refused.

While connected, text copied on either side can be pasted on the other (up to 1 MiB, plain text; only copies made during the session, and never logged). `aetherdesk --no-clipboard` turns that off both ways.

The Mac's sound plays on Linux (Opus, 48 kHz stereo, about 30 ms of buffer; more with `AETHERDESK_AUDIO_MS=60` on a jittery network). It needs no extra permission on the Mac: Screen & System Audio Recording covers it. `aetherdesk --no-audio` turns it off; the Mac then captures none.

When a session ends, the installed host locks the Mac: right away when you close the client, after 30 s when the link died (so a Wi-Fi hiccup doesn't cost you your password).

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
