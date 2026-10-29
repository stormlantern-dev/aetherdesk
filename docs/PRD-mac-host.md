# PRD — macOS host → Linux client

*Use a Mac from a Hyprland machine, including when the Mac is locked.* Shared principles and priority words: [PRD.md](PRD.md). How it was built, milestone by milestone: [MVP-PLAN.md](MVP-PLAN.md).

**State (October 2026):** built and released as `0.1.0-alpha.2` (Homebrew casks `aetherdesk` and `aetherdesk-host`). This document records what the direction is for, what it does and doesn't do, and what's still open, so the [reverse direction](PRD-linux-host.md) can be held against it.

## Summary

A Mac (the host) runs a root daemon plus an agent in each GUI session. A Linux client connects over QUIC, sees the Mac's screen in a window and controls it with keyboard and mouse. When the Mac is locked, the client sees the lock screen and types the password; macOS checks it. When the client leaves, the Mac locks.

## Goals

1. Work on the Mac from the Linux desk as if sitting at it, shortcuts included.
2. Get into a **locked** Mac without walking to it.
3. Nobody but paired machines can connect, and a Mac is never left unlocked by a disconnect.
4. Text stays sharp enough to read code all day.

## Non-goals

- **Logging in after a reboot.** FileVault's pre-boot screen runs before macOS; it stays on and stays local.
- **Logging in from the login window.** Since macOS 26.6.2 the login window ignores synthetic input from both a LoginWindow agent and a root daemon ([#3](https://github.com/stormlantern-dev/aetherdesk/issues/3)). The login window is view-only; macOS Screen Sharing remains the way in from there.
- **Headless Macs, virtual displays, several displays.** The main display is captured at its own size.
- **Internet access without a VPN.** No NAT traversal, no relay. LAN, Tailscale or WireGuard.
- **Audio, file transfer, images on the clipboard.** Audio is a separate idea ([#29](https://github.com/stormlantern-dev/aetherdesk/issues/29)).
- **Other clients.** The client is Linux only here; a Mac client belongs to the [reverse direction](PRD-linux-host.md).

## Constraints, with receipts

| Constraint | Source |
|---|---|
| Capture and input must run **in the GUI session** (agent), not in the root daemon: the daemon has no window server session, so ScreenCaptureKit times out. | M0 spike, [#3](https://github.com/stormlantern-dev/aetherdesk/issues/3) |
| ScreenCaptureKit keeps delivering frames while locked, and `CGEventPost` reaches the lock-screen password field despite Secure Event Input. | M0, [#3](https://github.com/stormlantern-dev/aetherdesk/issues/3) |
| The display sleeps about a minute after locking and capture then finds no display; the daemon must hold a user-activity power assertion while a client is connected. | M0, fixed in PR #20 |
| Screen Recording and Accessibility grants are tied to the code signature. Ad-hoc signatures change per build, so a stable self-signed identity is needed to keep grants across updates. | M0; `aether-agent install` re-signs |
| VideoToolbox can't encode AV1 on Apple Silicon. | [MVP-PLAN](MVP-PLAN.md#cut-from-the-mvp) |
| Locking uses `SACLockScreenImmediate`, a private symbol looked up at run time; Ctrl+Cmd+Q is the fallback. | [MVP-PLAN](MVP-PLAN.md#top-risks) |

## Requirements and their state

| # | Requirement | Prio | Fit criterion | State |
|---|---|---|---|---|
| H1 | Use a logged-in Mac from Hyprland. | Must | 10 minutes of work; Cmd+Tab, Cmd+Space, Cmd+Q act on the Mac; Super+Q doesn't close the client. | ✅ M1, [#7](https://github.com/stormlantern-dev/aetherdesk/issues/7) |
| H2 | Unlock a locked Mac remotely. | Must | Lock → connect → see the lock screen → type the password → desktop. | ✅ M2, [#18](https://github.com/stormlantern-dev/aetherdesk/issues/18) |
| H3 | Survive login/logout without reconnecting. | Should | Log out → connect → log in locally → the client follows into the desktop. | ✅ M2 (login-window input: see non-goals) |
| H4 | Only paired clients connect. | Must | An unpaired client gets nothing; an imposter host is refused. | ✅ M3, [#13](https://github.com/stormlantern-dev/aetherdesk/issues/13) |
| H5 | Lock the Mac when the client leaves. | Must | Quit, crash or network loss → the Mac is locked. | ✅ PR #36 |
| H6 | Find the Mac without an IP. | Should | `aetherdesk pair` on the LAN lists it. | ✅ PR #45 |
| H7 | Sharp text. | Should | HEVC 4:4:4 by default when the client's GPU decodes it. | ✅ PR #51 |
| H8 | Picture follows the client window size. | Should | Resizing the window changes the encoded size within a second. | ✅ PR #53 |
| H9 | Plain-text clipboard both ways. | Should | Multi-line text and emoji survive both directions. | ✅ [#23](https://github.com/stormlantern-dev/aetherdesk/issues/23) |
| H10 | Pointer drawn locally, without lag. | Should | The pointer follows the hand at the client's frame rate. | 🟡 Built (PR #48); warp and login-window cases untested, [#22](https://github.com/stormlantern-dev/aetherdesk/issues/22) |
| H11 | Configurable host key. | Could | `--host-key` changes the key; documented. | 🟡 Built (PR #55), [#24](https://github.com/stormlantern-dev/aetherdesk/issues/24) open |
| H12 | Installable without a repo checkout. | Should | `brew install --cask` + `aether-agent install`; permissions survive `brew upgrade`. | 🟡 Install and upgrade to `0.1.0-alpha.2` work; whether the grants survive the upgrade is being tested |

## Measurements

Capture to on-screen, median of 15 frames, measured by the client's per-stage split (PR #52). Mac: MacBook Pro M3 Pro. Client: RTX 4070 Ti.

| Path | Codec | Picture | Total | Of which encode |
|---|---|---|---|---|
| LAN | HEVC 4:2:0 | full resolution | ~41 ms | — |
| LAN | HEVC 4:4:4 | full resolution | 54.5 ms | 37 ms |
| WireGuard VPN | HEVC 4:4:4 | 1728×1116 (window size) | 36–40 ms | ~17 ms |

These are single sessions, not a benchmark. The comparison with Apple Screen Sharing, RustDesk and Pippin is [#33](https://github.com/stormlantern-dev/aetherdesk/issues/33).

## Success metrics

- The Mac stays usable from the Linux desk for a full working day without walking to it (unlock included).
- No session ends with the Mac unlocked.

## Open

- **Benchmark** ([#33](https://github.com/stormlantern-dev/aetherdesk/issues/33), run sheet [#40](https://github.com/stormlantern-dev/aetherdesk/issues/40)): numbers against the alternatives.
- **Recurring permission prompts** on macOS Tahoe: still being watched ([#3](https://github.com/stormlantern-dev/aetherdesk/issues/3)).
- **Distribution** with a Developer ID and notarization ([#28](https://github.com/stormlantern-dev/aetherdesk/issues/28)), so grants survive updates without a self-signed identity.
- **UI polish** ([#31](https://github.com/stormlantern-dev/aetherdesk/issues/31), [#34](https://github.com/stormlantern-dev/aetherdesk/issues/34)).

## What would change this plan

- **Apple closes synthetic input at the lock screen** the way it did for the login window. H2 would then need another way in (Screen Sharing's own channel, or nothing).
- **`SACLockScreenImmediate` disappears** and the Ctrl+Cmd+Q fallback stops working while the client is connected: H5 would need a new mechanism.
