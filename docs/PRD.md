# AetherDesk — Product Requirements Document

*A modern, secure, high-performance remote desktop for Wayland and macOS.*

## 1. Problem & Vision

### What's broken today

- **Game-streaming tools leave the machine exposed.** Sunshine and similar tools only work against a session that is already logged in and unlocked. They don't integrate with display managers (greetd, systemd-logind) or with macOS FileVault and the login window.
- **Keyboard shortcuts get eaten.** macOS swallows Cmd+Tab, Mission Control and Spotlight before they reach the remote machine; Hyprland does the same with `$mainMod`. There is no standard escape key for handing control back to the local machine.
- **Image quality suffers.** RDP and VNC are sluggish. NoMachine produces washed-out colors and blurry text because it never converts between color spaces (Display P3 vs. sRGB).
- **Headless setups don't scale.** Without a physical monitor attached, nothing negotiates the remote resolution to match the client.

### Vision

An open-source client/server remote desktop that:

- **Owns the session lifecycle securely** — starting and ending sessions itself, using PAM on Linux and native session management on macOS.
- **Streams straight from the GPU** — DMA-BUF and PipeWire on Linux, ScreenCaptureKit and Metal on macOS.
- **Makes the keyboard fully transparent** — through an explicit keyboard grab/release model, so every shortcut lands where the user expects it.

## 2. Architecture

```
                 ┌────────────────────────────────────────────────────────┐
                 │                 Network (WebRTC / QUIC)                │
                 │           H.264 / HEVC / AV1 + raw XKB input           │
                 └───────────────▲────────────────────────▲───────────────┘
                                 │                        │
     ┌───────────────────────────┴─────┐            ┌─────┴───────────────────────────┐
     │          HYPRLAND NODE          │            │            macOS NODE           │
     ├─────────────────────────────────┤            ├─────────────────────────────────┤
     │ Host:                           │            │ Host:                           │
     │ • Headless DRM / wlroots output │            │ • ScreenCaptureKit (zero-copy)  │
     │ • PipeWire DMA-BUF export       │            │ • VideoToolbox (HW encode)      │
     │ • PAM login daemon              │            │ • CGEventPost (synthetic input) │
     │ • zwp_virtual_keyboard_v1       │            │ • LaunchDaemon (root/pre-login) │
     │                                 │            │                                 │
     │ Client:                         │            │ Client:                         │
     │ • wp_keyboard_shortcuts_inhibit │            │ • CGEventTap (captures Cmd+Tab) │
     │ • OpenGL / Vulkan renderer      │            │ • Metal viewport rendering      │
     └─────────────────────────────────┘            └─────────────────────────────────┘
```

## 3. Functional Requirements

### A. Authentication & Session Lifecycle

| | Hyprland (Linux) | macOS |
|---|---|---|
| **Before login** | Runs as a system-level systemd daemon, listening over TLS. | Runs as a LaunchDaemon (`/Library/LaunchDaemons`) in the pre-login context. |
| **Authentication** | Verifies credentials through PAM — password, SSH key or U2F token. | Verifies credentials through the OpenDirectory (DirectoryService) API. |
| **Starting a session** | Creates a headless seat via systemd-logind and launches a dedicated `Hyprland -c /path/to/headless.conf`. | Switches to the target account via Fast User Switching, or presents the login window. |
| **Ending a session** | When the client disconnects, the session is locked immediately (hyprlock) or terminated. Physical monitors never show the active desktop. | When the client disconnects, the screen is locked via `SACLockScreenImmediate()`. |

### B. Capture & Video Pipeline

**Linux host → macOS client**

- **Capture:** `hyprctl output create headless` creates a virtual output at the client's exact resolution, including the Retina 2× scale factor.
- **Export:** Frames are handed to the hardware encoder (VA-API / NVENC) via PipeWire without copying.
- **Color:** The stream is tagged as sRGB or Rec. 709; the Mac renders it through Metal into the Display P3 color space, so gamma stays correct and colors don't look washed out.

**macOS host → Linux client**

- **Capture:** ScreenCaptureKit (macOS 13+) delivers `SCStream` frames straight into Metal textures, with no CPU copy.
- **Virtual display:** `CGVirtualDisplay` creates a virtual monitor, so a headless Mac doesn't need an HDMI dummy plug.
- **Encoding:** Hardware HEVC/AV1 encoding through VideoToolbox.

### C. Input, Shortcuts & the Escape Key

This is where existing tools are most frustrating, and where AetherDesk has to get it right.

#### Input modes

1. **Passthrough** (the default during a session)
   - *Hyprland client:* Enables `wp_keyboard_shortcuts_inhibit_v1`, so Hyprland forwards `$mainMod`, Super+Q and the rest untouched to the remote session.
   - *macOS client:* Installs a low-level `CGEventTap` (`kCGHIDEventTap`, `kCGHeadInsertEventTap`) that intercepts Cmd+Tab, Cmd+Space, Mission Control and Spaces before macOS acts on them, and forwards them as scancodes.
2. **Host escape**
   - A configurable key combination that won't clash with anything (e.g. Right Ctrl+Space, or Pause) instantly hands input back to the local machine.
   - A subtle 1px accent border around the window shows that input is now local.
3. **Key translation**
   - Maps physical keys to logical XKB keysyms and Mac virtual keycodes:
     - Linux → Mac: Super → Command, Alt → Option, Ctrl → Control.
     - Mac → Linux: Command → Super, Option → Alt.

## 4. Technology Choices

- **Language: Rust**
  - Fast, memory-safe and built for concurrency.
  - Mature bindings on Linux (`wayland-rs`, `pipewire-rs`, `pam-rs`) and macOS (`metal-rs`, `objc2`, `core-graphics`).
- **Transport: QUIC / WebRTC (DataChannels + RTP)**
  - Low latency, built-in congestion control, NAT traversal (STUN/TURN) and TLS 1.3 encryption out of the box.
- **Video codecs**
  - **AV1** (preferred): the best compression for text and sharp window edges.
  - **HEVC (H.265):** widely hardware-accelerated on Apple Silicon as well as AMD, Intel and Nvidia GPUs.

## 5. Scope: MVP (Phase 1) vs. V2

### MVP — fix the worst pain first

- [ ] **Linux daemon:** A standalone binary, started manually, that sets up a stream via wayvnc/PipeWire and injects input through `zwp_virtual_keyboard_v1`.
- [ ] **macOS client:** A minimal Metal/Cocoa viewer with a `CGEventTap` that captures Cmd+Tab, and a hard-coded host escape key (Right Ctrl).
- [ ] **Hyprland client:** A viewer with `wp_keyboard_shortcuts_inhibit_v1` enabled.
- [ ] **Key mapping:** A reliable one-to-one mapping between macOS modifiers (Cmd/Option) and Linux modifiers (Super/Alt).

### V2 — the full product

- [ ] PAM login service on Linux, so sessions can start from a headless login.
- [ ] macOS LaunchDaemon with `CGVirtualDisplay` support (no physical display required).
- [ ] Dynamic resolution: resizing the client window resizes the remote desktop on the fly.
- [ ] Clipboard sync (text and images) via Wayland data-control and macOS `NSPasteboard`.

## 6. Key Technical Risks

- **macOS permissions (TCC):** Running a `CGEventTap`, and using ScreenCaptureKit before login, both need Accessibility and Screen Recording permissions. At the login window there is often no user context to grant them to, so the LaunchDaemon needs root privileges and MDM-deployable configuration profiles.
- **Stability of Hyprland's headless outputs:** Creating virtual outputs with `hyprctl` works, but reconfiguring them on the fly (e.g. changing DPI) can crash certain Hyprland versions.
