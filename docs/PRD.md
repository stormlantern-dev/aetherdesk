# Product Requirements Document (PRD): Project "AetherDesk"

*Moderne, veilige, high-performance remote desktop voor Wayland & macOS*

## 1. Probleemstelling & Visie

### Huidige pijnpunten

- **Veiligheidslek bij game-streamers (Sunshine):** Vereist een ingelogde, ontgrendelde sessie. Geen integratie met display managers (greetd, systemd-logind) of macOS FileVault/Loginwindow.
- **Input Hell:** macOS slikt Cmd+Tab, Mission Control en Spotlight in; Hyprland slikt `$mainMod` in. Geen gestandaardiseerde host-key escape sequence.
- **Compressie & Kleur:** RDP/VNC zijn traag; NoMachine geeft fletse kleuren en slechte tekstweergave door ontbrekende kleurruimte-conversie (P3 vs sRGB).
- **Headless schaling:** Geen dynamische resolutie-onderhandeling bij remote verbinding zonder fysiek scherm.

### Visie

Een open-source client-server applicatie die:

- Veilig sessies spawnt en termineert (PAM-authenticatie op Linux, native sessiebeheer op macOS).
- GPU-native streaming levert (DMA-BUF + PipeWire op Linux, ScreenCaptureKit + Metal op macOS).
- Toetsenborden 100% transparant overbrugt via expliciete Keyboard Grab/Release protocollen.

## 2. Architectuur per Richting

```
                 ┌────────────────────────────────────────────────────────┐
                 │                 Netwerk (WebRTC / QUIC)                │
                 │           H.264 / HEVC / AV1 + Raw XKB Input           │
                 └───────────────▲────────────────────────▲───────────────┘
                                 │                        │
     ┌───────────────────────────┴─────┐            ┌─────┴───────────────────────────┐
     │          HYPRLAND NODE          │            │            macOS NODE           │
     ├─────────────────────────────────┤            ├─────────────────────────────────┤
     │ Host:                           │            │ Host:                           │
     │ • Headless DRM / wlroots output │            │ • ScreenCaptureKit (zero-copy)  │
     │ • PipeWire DMA-BUF export       │            │ • Apple VideoToolbox (HW enc)   │
     │ • PAM login daemon              │            │ • CGEventPost (synthetic input) │
     │ • zwp_virtual_keyboard_v1       │            │ • LaunchDaemon root/pre-login   │
     │                                 │            │                                 │
     │ Client:                         │            │ Client:                         │
     │ • wp_keyboard_shortcuts_inhibit │            │ • CGEventTap (intercept Cmd+Tab)│
     │ • OpenGL / Vulkan renderer      │            │ • Metal viewport rendering      │
     └─────────────────────────────────┘            └─────────────────────────────────┘
```

## 3. Functionele Specificaties

### A. Authenticatie & Sessie-Lifecycle (Veiligheid)

| Feature | Hyprland (Linux) | macOS |
|---|---|---|
| **Pre-login state** | Server draait als system-level daemon (systemd). Luistert over TLS. | Draait als LaunchDaemon (`/Library/LaunchDaemons`) in pre-login context. |
| **Authenticatie** | Valideert credentials via PAM (ondersteunt wachtwoord, SSH-key of U2F token). | Valideert via DirectoryService / OpenDirectory API. |
| **Sessie spawn** | Maakt headless seat aan via systemd-logind en start een dedicated `Hyprland -c /path/to/headless.conf`. | Schakelt via Fast User Switching naar de doelaccount of toont het loginwindow. |
| **Sessie beëindigen** | Client ontkoppelt → sessie vergrendelt direct (hyprlock) of termineert. Fysieke monitors tonen nooit de actieve desktop. | Client ontkoppelt → scherm vergrendelt via `SACLockScreenImmediate()`. |

### B. Display Capture & Video Pipeline (Beeld & Kleur)

**Linux → macOS**

- **Capture:** `hyprctl output create headless` genereert een virtuele display op de exacte resolutie van de Mac-client (incl. Retina 2× factor).
- **Export:** Zero-copy buffer handoff via PipeWire naar de hardware-encoder (VA-API / NVENC).
- **Kleur:** Signaleert sRGB of Rec.709 metadata; Mac rendert via Metal direct naar de P3-kleurruimte zonder fletse gamma.

**macOS → Linux**

- **Capture:** ScreenCaptureKit (macOS 13+) met `SCStream` direct in Metal GPU textures (geen CPU copy).
- **Virtuele monitor:** Maakt een virtueel display aan via `CGVirtualDisplay` (CoreGraphics private/public frameworks) zodat de Mac geen fysieke HDMI-dummy nodig heeft.
- **Encoding:** Hardwarematige HEVC/AV1 encoding via Apple VideoToolbox.

### C. Input, Sneltoetsen & De Escape Sequence

Dit is het cruciale onderdeel dat bestaande tools frustrerend maakt.

#### De drie input-modi

1. **Passthrough Mode** (standaard in sessie)
   - *Hyprland als client:* Activeert `wp_keyboard_shortcuts_inhibit_v1`. Hyprland geeft `$mainMod`, Super+Q, etc. ongefilterd door aan de remote sessie.
   - *macOS als client:* Installeert een low-level `CGEventTap` (`kCGHIDEventTap` op `kCGHeadInsertEventTap`). Vangt Cmd+Tab, Cmd+Space, Mission Control en Spaces af vóór macOS ze verwerkt en stuurt ze als scancodes door.
2. **Host Escape Mode**
   - Een niet-conflicterende key-combo (configureerbaar, bijv. Right-Ctrl + Space of Pause) switcht direct naar lokaal beheer.
   - Visuele overlay: subtiele rand (1px neon/accent) rond het scherm geeft aan dat input nu lokaal is.
3. **Key Translation Layer**
   - Mapt fysieke toetsen op logische XKB / Mac Virtual Keycodes:
     - Linux → Mac: Super → Command, Alt → Option, Ctrl → Control.
     - Mac → Linux: Command → Super, Option → Alt.

## 4. Technische Stack-Keuzes

- **Taal: Rust**
  - Hoge geheugensnelheid en concurrency.
  - Uitstekende bindings voor Linux (`wayland-rs`, `pipewire-rs`, `pam-rs`) en macOS (`metal-rs`, `objc2`, `core-graphics`).
- **Transportprotocol: QUIC / WebRTC DataChannels + RTP**
  - Lage latency, automatische congestiecontrole, NAT-traversal (STUN/TURN) en ingebouwde TLS 1.3 encryptie.
- **Video codecs**
  - **AV1** (prioriteit): beste compressie bij tekst/scherpe vensterranden.
  - **HEVC (H.265):** brede hardware-ondersteuning op zowel Apple Silicon als AMD/Intel/Nvidia.

## 5. Scope: MVP (Fase 1) vs. V2

### MVP scope (wat eerst gebouwd moet worden om de pijn te verhelpen)

- [ ] **Daemon (Linux):** Standalone binary die via wayvnc/PipeWire een stream opzet na handmatige start, maar mét `zwp_virtual_keyboard_v1`.
- [ ] **Client (macOS):** Eenvoudige Metal/Cocoa viewer met een `CGEventTap` die Cmd+Tab tegenhoudt en een hardcoded host-escape key bevat (Right-Ctrl).
- [ ] **Client (Hyprland):** Viewer met `wp_keyboard_shortcuts_inhibit_v1` geactiveerd.
- [ ] **Keymapping:** Betrouwbare 1:1 matrix tussen macOS modifiers (Cmd/Option) en Linux modifiers (Super/Alt).

### V2 scope (het volwaardige product)

- [ ] PAM login service voor Linux (starten vanaf headless login).
- [ ] macOS LaunchDaemon met `CGVirtualDisplay` ondersteuning (geen fysiek scherm nodig).
- [ ] Dynamische resolutiewijziging (resize client window = resize remote desktop resolutie on-the-fly).
- [ ] Clipboard synchronisatie (tekst, afbeeldingen) via Wayland data-control en macOS `NSPasteboard`.

## 6. Grootste Technische Risico's

- **macOS rechtenbeheer (TCC):** Om een `CGEventTap` te draaien en ScreenCaptureKit pre-login te gebruiken, zijn speciale toegankelijkheids- en schermopnamerechten nodig. In een pre-login sessie (Loginwindow) ontbreekt soms de gebruikerscontext; een LaunchDaemon heeft hiervoor root-privileges en MDM-compatibele profielen nodig.
- **Hyprland headless protocol stabiliteit:** Het programmatisch aanmaken van virtuele outputs via `hyprctl` werkt, maar herconfiguratie (DPI-wijzigingen on the fly) kan soms crashes triggeren in specifieke Hyprland-versies.
