# AetherDesk — MVP Plan

**Goal:** from a Hyprland machine, connect to a Mac that is locked (or logged out), log in by typing the password remotely, and end up in a working desktop session — keyboard (including Cmd+Tab), mouse and video.

**Target host:** MacBook Pro M3, macOS Tahoe 26.6, FileVault **on** (stays on; owner has full admin control, no restrictive MDM). That means a cold boot always needs local unlock at the FileVault pre-boot screen; remote login *after a reboot* is out of scope. Once the Mac has been unlocked once after boot, everything below applies.

This is the [macOS host → Linux client](PRD-mac-host.md) direction. The old combined PRD started with a Linux *host*; that direction now has [its own PRD](PRD-linux-host.md).

## Direction & shape

```
 Hyprland client (Rust, SDL3)                  macOS host (Rust, one binary)
 ┌───────────────────────────┐   QUIC (LAN)   ┌───────────────────────────────────────┐
 │ window + keyboard grab    │◄──────────────►│ aetherd daemon (root, LaunchDaemon)   │
 │ H.264 decode (ffmpeg)     │  video ◄       │  • only network listener, TLS/QUIC    │
 │ NV12 texture render       │  input ►       │  • relays to the active session agent │
 └───────────────────────────┘                │  • keeps the display awake while      │
                                              │    a client is connected              │
                                              │         ▲ unix socket                 │
                                              │ aether-agent (LaunchAgent, per GUI    │
                                              │ session: LoginWindow and Aqua)        │
                                              │  • ScreenCaptureKit → VideoToolbox    │
                                              │  • CGEventPost for input              │
                                              └───────────────────────────────────────┘
```

**Key idea: we don't implement authentication ourselves.** We stream the macOS lock screen / login window and forward keystrokes; macOS checks the password. No OpenDirectory, no Fast User Switching logic, no PAM.

Two cases, in order of importance for a laptop:

1. **Locked screen** (the common case): the user session keeps running, so the Aqua agent stays alive. Only capture + input on top of the lock screen must work.
2. **Logged out** (after logout, not after reboot): the LoginWindow agent shows the login window; after login, launchd starts the Aqua agent and the daemon switches the relay over to it.

## Cut from the MVP

| Cut | Why it's safe to drop for now |
|---|---|
| Linux host, macOS client | Not needed for "log in to the Mac". |
| WebRTC, STUN/TURN, NAT traversal | LAN / Tailscale only. Plain QUIC (`quinn`) is enough. |
| AV1 | VideoToolbox can't encode AV1 on current Apple Silicon. Use **H.264** (universal, best low-latency support in VT); HEVC later. |
| Hardware decode on the client | ffmpeg software decode of H.264 is fine at 1440p/60 on a desktop CPU. VA-API later. |
| Custom Metal/Vulkan renderer | SDL3 renders NV12 textures directly. |
| `CGVirtualDisplay`, headless Mac | Capture the existing main display at its native size; client letterboxes. |
| Dynamic resolution, multi-monitor | Single display, fixed size. |
| Clipboard, audio, file transfer | V2. |
| Color management (P3 ↔ sRGB) | Tag the stream as Rec.709 and accept it for now. |
| Login after reboot (FileVault pre-boot) | Impossible remotely, and FileVault stays on. `fdesetup authrestart` may work for a planned reboot but is not part of the MVP. |
| Installer, notarization, Developer ID | Manual install script; TCC permissions granted by hand once. No paid Apple account needed (see below). |
| User accounts / ACLs | Pairing instead: every paired client gets in, nobody else (M3). |

## What we keep, and why it's cheap

- **Keyboard passthrough on Hyprland:** `SDL_SetWindowKeyboardGrab` uses `zwp_keyboard_shortcuts_inhibit_v1` on Wayland, so `$mainMod` and friends go to the remote side. No hand-written Wayland code.
- **Key mapping:** the client sends **USB HID usage codes** (SDL scancodes already are those). The host maps HID → macOS virtual keycode in one table. Left/Right GUI (Super) → Command, Alt → Option falls out of that naturally.
- **Host escape:** hard-coded Right Ctrl, toggles the keyboard grab. Border/indicator in the window title only.
- **Code signing without paying:** TCC ties Screen Recording / Accessibility grants to the binary's code signature. Ad-hoc signing changes on every build, so grants get lost. A **self-signed code-signing certificate** (Keychain Access → Certificate Assistant) gives a stable signature across rebuilds — enough for running on your own Mac. The free "Apple Development" certificate from Xcode also works. Developer ID + notarization (paid) only matters for distributing to other people.

## Milestones

### M0 — Spikes ✅ GO

Done with [`spikes/m0`](../spikes/m0/) on the target Mac; details in [issue #3](https://github.com/stormlantern-dev/aetherdesk/issues/3). Both cases are a **go**.

| Question | Result |
|---|---|
| Agent in the user session captures the lock screen? | ✅ ScreenCaptureKit keeps delivering frames while locked. |
| Typing into the lock-screen password field? | ✅ `CGEventPost` from the agent works on both the HID and the session tap, despite Secure Event Input. |
| Root LaunchDaemon? | Typing ✅, capture ❌: it has no window server session, so ScreenCaptureKit times out. |
| LoginWindow agent after logout? | ✅ Starts (as root), captures the login window, typing reaches the password field. launchd kills it right after login. |
| Permissions survive rebuild + re-sign? | ✅ With the self-signed certificate, no re-grant needed. |
| Recurring "continue to allow?" prompts? | ⏳ Still being observed. |

**What changed in the design because of M0:**

- **Capture and input both live in the agents.** The daemon can't capture, and the agent can already type, so the daemon only does networking and session switching.
- **The daemon keeps the display awake.** About a minute after locking, the display sleeps and ScreenCaptureKit reports *no displays*. While a client is connected, the daemon holds a power assertion that wakes the display and keeps it on (IOKit `IOPMAssertionDeclareUserActivity` + `PreventUserIdleDisplaySleep`).
- **Handoff at login is required**, not optional: the LoginWindow agent is gone two seconds after login.

### M1 — Use a logged-in Mac from Hyprland

- Cargo workspace: `proto` (messages, `postcard`), `mac-host`, `linux-client`.
- Agent run by hand in a Terminal on a logged-in Mac; listens directly (no daemon yet).
- SCK frames → VideoToolbox H.264 (low-latency rate control, keyframe on request) → length-prefixed NAL units on one QUIC stream.
- Input on a second QUIC stream: key down/up (HID code), absolute mouse position (normalized), buttons, scroll.
- Client: SDL3 window, ffmpeg decode, NV12 texture, keyboard grab, Right Ctrl escape.

**Done when:** you can work on the Mac from Hyprland for 10 minutes; Cmd+Tab, Cmd+Space and Cmd+Q act on the Mac; Super+Q does not close the client window.

### M2 — Log in remotely

- Split host into `aetherd` (root LaunchDaemon, QUIC listener) and `aether-agent` (LaunchAgent for both `LoginWindow` and `Aqua` session types), talking over a unix socket in `/var/run/aetherdesk/`.
- Lock screen: nothing special — the Aqua agent keeps running, and capture + input work on top of the lock screen (M0).
- Logged out: daemon picks the agent of the console session; when the console session changes (login), it switches the relay and requests a keyframe. Client connection stays up.
- Daemon wakes the display on connect and keeps it awake until disconnect.
- Install script for the plists + binaries.

**Done when:**
- Lock the Mac → connect from Hyprland → see the lock screen → type the password → land in the desktop.
- Log out → connect → see the login window → log in → land in the desktop without reconnecting.

### M3 — Pairing and minimum security

Connecting should be one command, and only paired machines get in. Until M3 the host used a shared token and a fresh certificate on every start, and the client accepted any certificate. Tracked in [issue #13](https://github.com/stormlantern-dev/aetherdesk/issues/13).

- **Persistent identities:** the host keeps one self-signed certificate across restarts (stored by the daemon from M2); each client has its own client certificate.
- **Pairing, once:** `scripts/mac.zsh pair` on the Mac shows a 6-digit code in the terminal (the daemon has no screen of its own) → `aetherdesk pair <host>` on Linux asks for it. The code goes through SPAKE2, and the confirmation is bound to both TLS certificates, so someone in the middle gets one guess per attempt. Both sides store the other's certificate fingerprint and a name. Codes expire after 2 minutes; three wrong codes cancel the attempt.
- **Connecting:** `aetherdesk <name>` with mutual TLS — the client only accepts the paired host certificate, the host only paired client certificates. No token, no fingerprint to copy.
- **Discovery:** the host announces itself over Bonjour/mDNS (`_aetherdesk._udp`), so names work on the LAN. A Tailscale name or an IP still works.
- **Unpairing** from either side.
- **Lock the screen** when the client disconnects.
- Stream and input are never accepted before the client is authenticated.

**Done when:** after one pairing, `aetherdesk <name>` connects without IP, token or fingerprint; an unpaired client gets nothing; disconnecting leaves the Mac locked.

## Top risks

1. **Recurring Screen Recording prompts** — if Tahoe periodically asks to re-confirm, someone has to click it at the Mac. Still being observed ([issue #3](https://github.com/stormlantern-dev/aetherdesk/issues/3)).
2. **Lock-screen API** — locking on disconnect uses `SACLockScreenImmediate`, which is private (looked up at run time); if it's gone, the agent posts Ctrl+Cmd+Q instead.
3. **Login window input** — since macOS 26.6.2 the login window ignores synthetic input from both a LoginWindow agent and a root daemon ([issue #3](https://github.com/stormlantern-dev/aetherdesk/issues/3)). The login window is view-only; logging in from there goes through macOS Screen Sharing.

Resolved by M0: capture and input at the lock screen; permissions for a self-signed binary in both the user and the loginwindow session. Resolved by M2: waking the display from the root daemon (an IOKit user-activity assertion) and the login/logout handoff (a short frozen frame, no reconnect).
