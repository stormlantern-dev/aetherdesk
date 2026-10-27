# AetherDesk — MVP Plan

**Goal:** from a Hyprland machine, connect to a Mac that is locked (or logged out), log in by typing the password remotely, and end up in a working desktop session — keyboard (including Cmd+Tab), mouse and video.

**Target host:** MacBook Pro M3, macOS Tahoe 26.6, FileVault **on** (stays on; owner has full admin control, no restrictive MDM). That means a cold boot always needs local unlock at the FileVault pre-boot screen; remote login *after a reboot* is out of scope. Once the Mac has been unlocked once after boot, everything below applies.

This deliberately differs from the MVP in [PRD §5](PRD.md#5-scope-mvp-phase-1-vs-v2), which starts with a Linux *host*. Here the Mac is the host and Hyprland is the client.

## Direction & shape

```
 Hyprland client (Rust, SDL3)                     macOS host (Rust, one binary)
 ┌───────────────────────────┐   QUIC (LAN)   ┌──────────────────────────────────────┐
 │ window + keyboard grab    │◄──────────────►│ aetherd daemon (root, LaunchDaemon)   │
 │ H.264 decode (ffmpeg)     │  video ◄       │  • only network listener, TLS/QUIC   │
 │ NV12 texture render       │  input ►       │  • relays to the active session agent│
 └───────────────────────────┘                │        ▲ unix socket                 │
                                              │ aether-agent (LaunchAgent, per GUI    │
                                              │ session: LoginWindow and Aqua)        │
                                              │  • ScreenCaptureKit → VideoToolbox    │
                                              │  • CGEventPost for input              │
                                              └──────────────────────────────────────┘
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
| User accounts / ACLs | One pre-shared token + pinned self-signed cert. |

## What we keep, and why it's cheap

- **Keyboard passthrough on Hyprland:** `SDL_SetWindowKeyboardGrab` uses `zwp_keyboard_shortcuts_inhibit_v1` on Wayland, so `$mainMod` and friends go to the remote side. No hand-written Wayland code.
- **Key mapping:** the client sends **USB HID usage codes** (SDL scancodes already are those). The host maps HID → macOS virtual keycode in one table. Left/Right GUI (Super) → Command, Alt → Option falls out of that naturally.
- **Host escape:** hard-coded Right Ctrl, toggles the keyboard grab. Border/indicator in the window title only.
- **Code signing without paying:** TCC ties Screen Recording / Accessibility grants to the binary's code signature. Ad-hoc signing changes on every build, so grants get lost. A **self-signed code-signing certificate** (Keychain Access → Certificate Assistant) gives a stable signature across rebuilds — enough for running on your own Mac. The free "Apple Development" certificate from Xcode also works. Developer ID + notarization (paid) only matters for distributing to other people.

## Milestones

### M0 — Spikes (go / no-go, ~days)

The risky parts are all on the Mac. Prove them with throwaway code before building anything:

1. **Capture of the lock screen:** an agent in the user session captures via ScreenCaptureKit while the screen is locked. Does it get the lock screen, or black frames?
2. **Typing into the lock screen:** the password field turns on Secure Event Input. Check whether `CGEventPost` from the user agent gets through, or only from the root daemon (`kCGHIDEventTap`).
3. **Same for the login window** after logout: LaunchAgent with `LimitLoadToSessionType = LoginWindow`, SCK capture + `CGEventPost`. Fall back to `CGDisplayStream` if SCK refuses outside a user session.
4. **TCC survives rebuilds** with the self-signed certificate.
5. **Recurring Screen Recording prompts:** recent macOS versions periodically ask "continue to allow?" for screen capture. If Tahoe does that for our agent, someone has to click it locally — check whether it shows up and how often.

If (1)/(2) fail, there's no MVP in this form; stop and reconsider. If only (3) fails, drop the logged-out case and ship the lock-screen case alone.

### M1 — Use a logged-in Mac from Hyprland

- Cargo workspace: `proto` (messages, `postcard`), `mac-host`, `linux-client`.
- Agent run by hand in a Terminal on a logged-in Mac; listens directly (no daemon yet).
- SCK frames → VideoToolbox H.264 (low-latency rate control, keyframe on request) → length-prefixed NAL units on one QUIC stream.
- Input on a second QUIC stream: key down/up (HID code), absolute mouse position (normalized), buttons, scroll.
- Client: SDL3 window, ffmpeg decode, NV12 texture, keyboard grab, Right Ctrl escape.

**Done when:** you can work on the Mac from Hyprland for 10 minutes; Cmd+Tab, Cmd+Space and Cmd+Q act on the Mac; Super+Q does not close the client window.

### M2 — Log in remotely

- Split host into `aetherd` (root LaunchDaemon, QUIC listener) and `aether-agent` (LaunchAgent for both `LoginWindow` and `Aqua` session types), talking over a unix socket in `/var/run/aetherdesk/`.
- Lock screen: input goes through whichever process M0 showed can reach the password field.
- Logged out: daemon picks the agent of the console session; when the console session changes (login), it switches the relay and requests a keyframe. Client connection stays up.
- Install script for the plists + binaries.

**Done when:**
- Lock the Mac → connect from Hyprland → see the lock screen → type the password → land in the desktop.
- Log out → connect → see the login window → log in → land in the desktop without reconnecting.

### M3 — Minimum security

- Pinned self-signed server cert (fingerprint in client config) + pre-shared token on connect.
- Lock the screen when the client disconnects.
- Stream and input are never accepted before token check.

**Done when:** a client without the token gets nothing; disconnecting leaves the Mac locked.

## Top risks

1. **Capture / input at the lock screen and login window** — the whole goal depends on it; that's why M0 exists.
2. **TCC for a root daemon / loginwindow agent** — permissions may need to be granted to the agent binary specifically, and may not show a prompt at all. Document the manual steps.
3. **Session handoff timing** — the window between loginwindow agent exiting and the Aqua agent starting will show a frozen frame. Acceptable for MVP.
4. **Lock-screen API** — `SACLockScreenImmediate` is private; fallback is triggering Ctrl+Cmd+Q via `CGEventPost`.
