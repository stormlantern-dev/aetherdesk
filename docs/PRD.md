# AetherDesk — Product Requirements

*A secure, low-latency remote desktop between Wayland (Hyprland) and macOS.*

This file holds what both directions share. Each direction has its own PRD:

| Direction | PRD | State |
|---|---|---|
| **macOS host → Linux client** | [PRD-mac-host.md](PRD-mac-host.md) | Built: M0–M3 done, released as `0.1.0-alpha` (Homebrew casks). |
| **Linux (Hyprland) host → macOS client** | [PRD-linux-host.md](PRD-linux-host.md) | Proposed. Nothing built yet. |

They were one document until October 2026. The first direction got built, and the decisions it forced (no WebRTC, no AV1, no own authentication) changed enough that the old combined text no longer described either direction well.

## 1. Problem

- **Game-streaming tools leave the machine exposed.** Sunshine and similar tools only work against a session that is already logged in and unlocked, and they don't lock it again when you leave.
- **Keyboard shortcuts get eaten.** macOS swallows Cmd+Tab, Mission Control and Spotlight before they reach the remote machine; Hyprland does the same with `$mainMod`. There is no standard escape key for handing control back to the local machine.
- **Image quality suffers.** RDP and VNC are sluggish, and chroma subsampling blurs text. NoMachine's colors look washed out.
- **Nothing is made for this pair of systems.** Tools that support both macOS and Wayland treat Wayland as an afterthought, or need a vendor account.

## 2. Principles (both directions)

1. **The host's own lock screen is the authentication.** We stream the lock screen and forward keystrokes; the host's OS checks the password. We don't implement PAM, OpenDirectory or account switching ourselves. *(Decided in [MVP-PLAN](MVP-PLAN.md); it held up in M2.)*
2. **Only paired machines get in.** A one-time 6-digit code (SPAKE2, bound to both TLS certificates), then mutual TLS with pinned certificates. No accounts, no relay server.
3. **Leave the host locked.** When a client leaves, for whatever reason, the host locks.
4. **The keyboard is transparent while grabbed.** Every shortcut goes to the remote side, except one host key that hands control back.
5. **Latency is measured per stage**, not guessed. The client can show capture, encode, network and decode separately.
6. **One protocol, one implementation of it.** Both directions use the same `proto` crate: QUIC, pairing, mDNS discovery, codec negotiation, clipboard, pointer. A direction adds a host and a client, not a second protocol.

## 3. Shared technology choices

| Choice | Why | Instead of |
|---|---|---|
| **Rust** workspace (`proto`, hosts, clients) | One language on both systems, safe concurrency, good bindings (`objc2`, `wayland-rs`, `ffmpeg`). | — |
| **QUIC** (`quinn`), own protocol (`postcard`) | Low latency, TLS 1.3 built in, one connection for video, input and control. Works over a VPN, with an IP fragmentation fallback for narrow paths (#58). | WebRTC: needs signalling and STUN/TURN for NAT traversal we don't need on a LAN or VPN. |
| **H.264, HEVC 4:2:0, HEVC 4:4:4**, negotiated in the handshake | Hardware encode and decode exist on both sides. HEVC 4:4:4 keeps text sharp. | AV1: VideoToolbox can't *encode* it, so it can't be the shared default. |
| **mDNS** (`_aetherdesk._udp`) for discovery | `aetherdesk pair` finds hosts on the LAN without an IP. | — |

## 4. Priority words

The direction PRDs use **MoSCoW**: **Must** (the phase isn't done without it), **Should** (expected, can slip one phase with a reason), **Could** (if cheap), **Won't** (decided not to, for now; listed so it doesn't come back as a new idea).
