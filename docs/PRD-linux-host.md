# PRD — Linux (Hyprland) host → macOS client

*Use a Hyprland machine from a Mac, including when it is locked.* Shared principles, technology and priority words: [PRD.md](PRD.md). The built direction this mirrors: [PRD-mac-host.md](PRD-mac-host.md).

**State (October 2026):** proposed. Nothing is built. The receipts below come from reading Hyprland's and SDL's source at the versions we use, from Apple's SDK headers, and from short measurements on our own machines. Everything else is marked as a budget, an assumption or a spike.

## The decision this document is for

1. **Build it, and in what order.** Each phase below is useful on its own; the spikes (L0) say whether L1 and L2 are possible at all.
2. **Which architecture for phase 1:** a host that runs **in the user's Hyprland session** and streams it, as opposed to a root daemon with PAM and its own headless session (the old PRD). This document proposes the first and records why.

## Summary

A Hyprland machine (the host) runs `aetherdesk-host` as a systemd *user* service inside the logged-in session. It captures an output through a Wayland capture protocol, encodes on the GPU and injects input through virtual keyboard and pointer protocols. A Mac client, built from the same code as today's Linux client, shows the picture and forwards the keyboard, Cmd+Tab included. When the Hyprland session is locked, the client sees the lock screen (hyprlock) and types the password; PAM, through hyprlock, checks it. When the client leaves, the session locks.

This is the Mac host's model turned around: the host's own lock screen is the authentication, pairing decides who may connect.

## Goals

1. Work on the Linux machine from a Mac as if sitting at it: `$mainMod` shortcuts reach Hyprland, Cmd+Tab doesn't switch Mac apps.
2. Get into a **locked** Hyprland session without walking to it.
3. The same security as the Mac host: only paired clients, and the session is never left unlocked by a disconnect.
4. Reuse: the same protocol, pairing and discovery, and as much of the existing client as works on macOS.

## Non-goals

- **Logged-out machines, the greeter, starting a session remotely.** The greeter (greetd, SDDM, …) is a separate compositor per display manager. Phase 1 needs a logged-in session that may be locked. A later phase can revisit this.
- **Own authentication** (PAM, SSH keys, U2F in our code). hyprlock already does PAM.
- **Compositors other than Hyprland.** GNOME and KDE need the RemoteDesktop portal or their own APIs; not now.
- **A separate headless session per connection** (the old PRD's `Hyprland -c headless.conf` via logind). It's a different product: a terminal server.
- **Internet access without a VPN, audio, file transfer, several displays at once.** Same as the Mac host.
- **Mac App Store distribution.** An event tap and Accessibility don't fit the sandbox.

## Constraints and findings, with receipts

### Hyprland (0.56.2, the version on the Linux machine)

| Finding | Receipt |
|---|---|
| The capture and input protocols we'd use are compiled in: `ext_image_copy_capture_manager_v1` with `ext_output_image_capture_source_manager_v1`, `zwlr_screencopy_manager_v1`, `zwp_virtual_keyboard_manager_v1`, `zwlr_virtual_pointer_manager_v1`, plus `ext_session_lock_manager_v1` and `zwp_keyboard_shortcuts_inhibit_manager_v1`. | Interface names in `/usr/bin/Hyprland` (`strings`). Not yet checked against what the running compositor advertises (`wayland-info` isn't installed). |
| Hyprland has a permission system, **off by default**: `ecosystem:enforce_permissions`, default `false`. | `src/config/values/ConfigValues.cpp:620` at `v0.56.2` |
| When it's on, screen capture is checked **per client program** (`PERMISSION_TYPE_SCREENCOPY`), and virtual keyboards go through the same per-device keyboard check as real ones (`PERMISSION_TYPE_KEYBOARD`, by device name). Rules match a binary path, e.g. `hl.permission("/usr/(lib\|libexec\|lib64)/xdg-desktop-portal-hyprland", "screencopy", "allow")` in the shipped example. | `src/managers/screenshare/ScreenshareFrame.cpp:154`, `src/managers/input/InputManager.cpp:1127–1244` |
| Virtual outputs exist: `hyprctl output create headless [name]`. | `src/debug/HyprCtl.cpp:1777–1784` |
| The xdg portal for Hyprland (`xdg-desktop-portal-hyprland` 1.4.1) offers `Screenshot`, `ScreenCast`, `GlobalShortcuts` and `InputCapture`, **not `RemoteDesktop`**. | `/usr/share/xdg-desktop-portal/portals/hyprland.portal`, `Interfaces=` line |
| **Not verified:** whether a capture of a locked output shows the lock screen, whether virtual-keyboard input reaches hyprlock, and whether capture works while the output is off (DPMS). The screenshare code has no lock-specific branch, which suggests capture shows whatever is rendered, lock screen included. | Spikes S1–S3 |

### GPU encode on the host (RTX 4070 Ti, driver 615.78)

Throughput, not latency: 600 frames of 2560×1440 `testsrc2` through `ffmpeg -c:v <enc> -preset p1 -tune ull`, wall time including frame generation and upload on the CPU.

| Encoder | Wall time for 600 frames |
|---|---|
| `h264_nvenc` 4:2:0 | 1.31 s |
| `hevc_nvenc` 4:2:0 | 0.92 s |
| `hevc_nvenc` 4:4:4 (`rext`) | 1.57 s |
| `av1_nvenc` 4:2:0 | 0.95 s |
| `av1_nvenc` 4:4:4 | fails: *Error while opening encoder* |

All supported formats run several times faster than 60 fps. Per-frame encode latency, the number that matters, is spike S5.

### macOS client (MacBook Pro M3 Pro, macOS 27)

| Finding | Receipt |
|---|---|
| VideoToolbox decodes **HEVC 4:2:0, HEVC 4:4:4 (Rext) and AV1** in hardware: with `-hwaccel videotoolbox` all three land in `videotoolbox_vld` surfaces. | `ffmpeg` 9.0.2 on the Mac, 1080p samples encoded by NVENC above |
| Grabbing Cmd+Tab needs an **event tap at session level with Accessibility**. Apple: *"Taps may only be placed at `kCGHIDEventTap` by a process running as the root user."* Session-level taps only receive key events if the process has Accessibility. The old PRD's `kCGHIDEventTap` is therefore wrong for a normal client. | `CoreGraphics.framework/Headers/CGEvent.h`, comment on `CGEventTapCreate` |
| **SDL's keyboard grab doesn't help on macOS.** SDL 3.4.18's `Cocoa_SetWindowKeyboardGrab` calls the private `CGSSetGlobalHotKeyOperatingMode` only when SDL is built with `SDL_MAC_NO_SANDBOX`, which the build system doesn't set. Otherwise it does nothing. | `src/video/cocoa/SDL_cocoakeyboard.m:596` at `release-3.4.18` |
| **Not verified:** whether a session tap can swallow Cmd+Tab, Cmd+Space and Mission Control on macOS 27 before the system acts on them. | Spike S4 |

## Proposed shape

```
 macOS client (Rust, SDL3)                       Hyprland host (Rust, systemd --user)
 ┌──────────────────────────────┐   QUIC        ┌─────────────────────────────────────────┐
 │ window, VideoToolbox decode  │◄─────────────►│ aetherdesk-host, in the Hyprland session │
 │ session event tap (Cmd+Tab)  │  video ◄      │  • ext-image-copy-capture → NVENC/VA-API │
 │ host key hands control back  │  input ►      │  • virtual keyboard + virtual pointer    │
 │ pairing, mDNS, clipboard     │               │  • wakes outputs, locks on disconnect    │
 └──────────────────────────────┘               │  • pairing store in the user's config    │
                                                └─────────────────────────────────────────┘
```

Why no root daemon, unlike the Mac: on the Mac the daemon exists for the login window and the power assertion. Phase 1 here has no greeter, and the locked session is the same compositor and the same user, so a user service survives locking. The cost: the host's key and pairings live in the user's config, readable by the user's own processes. Those could already drive the session, so this adds no new access.

## Requirements

| # | Requirement | Prio | Fit criterion |
|---|---|---|---|
| R1 | Use a logged-in Hyprland session from a Mac. | Must | 10 minutes of work; `$mainMod` shortcuts act on Hyprland; Cmd+Tab and Cmd+Space don't act on the Mac while grabbed. |
| R2 | A host key hands the keyboard back to the Mac. | Must | Pressing it releases the tap; Cmd+Tab switches Mac apps again; pressing it again grabs. |
| R3 | Only paired clients connect. | Must | Same tests as the Mac host (unpaired client refused, imposter host refused), run against the Linux host. |
| R4 | Unlock a locked session remotely. | Must | Lock (hyprlock) → connect → see the lock screen → type the password → desktop. |
| R5 | Lock the session when the client leaves. | Must | Quit, crash or network loss → the session is locked within 5 s. |
| R6 | Wake the outputs on connect. | Must | After the outputs have gone off (DPMS), connecting shows a picture within 2 s. |
| R7 | Latency close to the Mac host's. | Should | Capture to on-screen, median of 15 frames, LAN, HEVC 4:2:0 at 1440p: **budget ≤ 45 ms** (the Mac host measured ~41 ms). Not measured: nothing is built. |
| R8 | Sharp text. | Should | HEVC 4:4:4 by default when both sides support it (they do on this hardware, see above). |
| R9 | Pairing and discovery as on the Mac. | Should | `aetherdesk pair` on the Mac finds the Linux host over mDNS; a code shown by the host pairs it. |
| R10 | Plain-text clipboard both ways. | Should | Multi-line text and emoji survive both directions. |
| R11 | Installable without building. | Should | One package per side: the client as a Homebrew cask on the Mac, the host from the Linux release tarball or PKGBUILD ([#37](https://github.com/stormlantern-dev/aetherdesk/issues/37)). |
| R12 | Picture size follows the client window, through a headless output. | Could | Resizing the client window resizes a `hyprctl output create headless` output within a second, without crashing Hyprland. |
| R13 | Works on AMD/Intel GPUs (VA-API), not just NVIDIA. | Could | The same fit as R7 on a VA-API machine. |
| R14 | Colors right on the Mac's P3 display. | Could | A sRGB test pattern looks the same on the Mac client as on the Linux monitor, side by side. |
| R15 | Works with `enforce_permissions = true`. | Could | With permissions enforced, documented rules for our binary make capture and input work without prompts. |

## Phasing

- **L0 — spikes.** Answer what L1 and L2 depend on, like M0 did for the Mac:
  - **S1** Capture a locked output with `ext-image-copy-capture` while hyprlock runs: picture of the lock screen, or black?
  - **S2** Type into hyprlock through `zwp_virtual_keyboard_v1`: does it unlock?
  - **S3** Capture after DPMS off, and wake the outputs (`hyprctl dispatch dpms on`) from the host.
  - **S4** On the Mac: a session event tap with Accessibility swallows Cmd+Tab, Cmd+Space, Mission Control (Ctrl+Up) and Cmd+Q, and lets them through again on release.
  - **S5** Per-frame latency: capture → NVENC → bitstream at 1440p, 4:2:0 and 4:4:4.
  - **S6** Today's client builds and runs on macOS (SDL3 + ffmpeg with VideoToolbox) against the Mac host or the fake host.
- **L1 — use a logged-in Hyprland from the Mac.** R1, R2, R3 (with the existing pairing), R7. The client is today's client plus a macOS event tap.
- **L2 — locked sessions.** R4, R5, R6, then R9 and R10.
- **L3 — fit and finish.** R8, R11, then the Coulds.

## Success metrics

- After L2: the Linux machine is used from the Mac at least weekly for real work, and nobody walks to it to unlock.
- No session ends with the Linux machine unlocked.

## Ruled out, and why

| Option | Why not |
|---|---|
| **Portal capture and input** (`ScreenCast` + `RemoteDesktop`, libei) | The Hyprland portal has no `RemoteDesktop`, so input would need the virtual keyboard anyway. `ScreenCast` goes through a share picker per session, which doesn't suit an unattended host (restore tokens might avoid it; not tested). Still the route for GNOME/KDE later. |
| **wayvnc as the host** (the old PRD's MVP) | A second protocol with its own authentication next to ours: no pairing, no codec negotiation, no shared client. |
| **Root daemon + PAM + own headless session** (the old PRD) | Breaks principle 1 (the host's lock screen is the authentication), and it's a terminal server, not remote access to *your* desktop. |
| **`kCGHIDEventTap` on the Mac** | Root only (`CGEvent.h`). |
| **SDL's keyboard grab on the Mac** | A no-op unless SDL is built with `SDL_MAC_NO_SANDBOX`, and then it relies on a private API. Fallback if S4 fails, not the plan. |
| **AV1 as the default** | NVENC encodes AV1 4:2:0 and the M3 decodes it, but NVENC can't do AV1 4:4:4, and HEVC 4:4:4 works on both sides. AV1 4:2:0 could be an extra codec later. |

## Open questions

| Question | Owner | Settled by |
|---|---|---|
| Where will the Mac be when this is used: the couch, or travelling over the VPN? It decides how much R12 (window-sized picture) matters. | Maintainer | Answer |
| While a client is connected, the physical monitors show the desktop too. Acceptable at home, or should the host turn them off (and how, without the session noticing)? | Maintainer | Answer, then a spike |
| Lock on disconnect: `loginctl lock-session` (needs hypridle or another handler listening), or start hyprlock directly? | Claude | S1/S2 plus the Linux machine's config |
| One client crate for Linux and macOS, or a macOS client of its own? | Claude | S6 |
| Which permission prompts does the Mac client hit (Accessibility, perhaps Input Monitoring), and do they survive updates with an ad-hoc signed cask? | Claude | S4, then a `brew upgrade` test |

## What would change this plan

- **S1 or S2 fails** (the lock screen can't be captured or typed into): R4 is impossible in this shape. Options then: a Hyprland plugin, or dropping the locked case and keeping only R5. Not: disabling the lock while connected.
- **S4 fails** (macOS 27 won't let a session tap swallow Cmd+Tab): R1's fit criterion loses Cmd+Tab, or we accept SDL's private-API route.
- **Hyprland turns `enforce_permissions` on by default:** R15 becomes a Must, and the install has to add rules.
- **S5 shows encode latency far above the Mac's** (~17–37 ms): R7's budget is wrong, and zero-copy capture (DMA-BUF into NVENC) moves from nice-to-have to a requirement.
