## Benchmark results (#33)

Mac: MacBook Pro M3, macOS <version>, 3456×2234, on <Wi-Fi / Ethernet>. Client: CachyOS, Hyprland, NVIDIA <model>, 2560×1440 @ 60 Hz, on <Ethernet>. Date: <date>. Method: `docs/benchmark.md`. Every client fullscreen on 2560×1440, scaled to Fit.

### Versions and non-default settings

| Tool | Version (Linux) | Version (Mac) | Codec / mode seen | Non-default settings |
|---|---|---|---|---|
| AetherDesk | `main` @ <commit> | `main` @ <commit>, installed | H.264 | none |
| Pippin | <version / commit> | macOS Screen Sharing | HEVC 4:4:4? | `--no-auto-resize` |
| Apple VNC (Remmina) | Remmina <version> | macOS Screen Sharing | VNC <encoding> | |
| RustDesk | <version> | <version> | <codec> | direct IP access on |
| Moonlight / Sunshine | <version> | <version> | | |
| Parsec | <version> | <version> | | |

### Latency (glass to glass, stopwatch photos)

| Tool | Photos | Median | Min–max | Notes |
|---|---|---|---|---|
| AetherDesk | | ms | | client's own capture→present: ms |
| Pippin | | ms | | |
| Apple VNC | | ms | | |
| RustDesk | | ms | | |
| Moonlight | | ms | | |
| Parsec | | ms | | |

### Smoothness (10 s, `fps.nu`)

| Tool | Scroll: distinct fps | Scroll: longest stall | Drag: distinct fps | Drag: longest stall | Tool's own fps | Stutter / tearing seen |
|---|---|---|---|---|---|---|
| AetherDesk | | ms | | ms | | |
| Pippin | | ms | | ms | | |
| Apple VNC | | ms | | ms | – | |
| RustDesk | | ms | | ms | | |
| Moonlight | | ms | | ms | | |
| Parsec | | ms | | ms | | |

### Mac CPU during the scroll test (`mac-cpu.nu`, 100 % = one core)

| Tool | Processes | Avg % | Peak % | WindowServer avg % |
|---|---|---|---|---|
| AetherDesk | aether-agent (daemon + agent) | | | |
| Pippin | screensharingd … | | | |
| Apple VNC | screensharingd … | | | |
| RustDesk | RustDesk / rustdesk | | | |
| Moonlight | sunshine | | | |
| Parsec | parsecd | | | |

### Picture quality (terminal, 11 pt)

| Tool | Sharp at 11 pt? | Colour fringes (4:2:0)? | Other artefacts | Screenshot |
|---|---|---|---|---|
| AetherDesk | | | | |
| Pippin | | | | |
| Apple VNC | | | | |
| RustDesk | | | | |
| Moonlight | | | | |
| Parsec | | | | |

### Packet loss and jitter (`netem.nu`, both directions, 60 s each)

Per cell: freezes? longest freeze · recovers on its own? · input arrives? E.g. `none` or `3×, 2 s · yes · yes`.

| Tool | 0 % | 1 % | 3 % | 5 % | 30 ± 10 ms |
|---|---|---|---|---|---|
| AetherDesk | | | | | |
| Pippin | | | | | |
| Pippin (`--after-loss conceal`) | | | | | |
| Apple VNC | | | | | |
| RustDesk | | | | | |
| Moonlight | | | | | |
| Parsec | | | | | |

### 10-s network interruption (`netem.nu cut`)

| Tool | Time to usable picture after restore | User action needed | Notes |
|---|---|---|---|
| AetherDesk | s | | |
| Pippin | s | | |
| Apple VNC | s | | |
| RustDesk | s | | |
| Moonlight | s | | |
| Parsec | s | | |

### Login window and locked Mac

| Tool | Lock screen (Ctrl+Cmd+Q) | Login window (logged out) | Notes |
|---|---|---|---|
| AetherDesk | | | |
| Pippin | | | |
| Apple VNC | | | |
| RustDesk | | | |
| Moonlight | | | |
| Parsec | | | |

### Summary (for the README)

| | AetherDesk | Pippin | Apple VNC | RustDesk |
|---|---|---|---|---|
| Latency (median) | | | | |
| Scroll fps / longest stall | | | | |
| Mac CPU (scroll) | | | | |
| Small text | | | | |
| 1 % loss | | | | |
| 5 % loss | | | | |
| Back after 10-s cut | | | | |
| Lock screen / login window | | | | |
| Linux install | source / AUR (#37) | build from source | repo (Remmina) | AUR |

### Where AetherDesk is behind

- <finding> → #<new issue>

<details><summary>Raw notes</summary>

Per-photo latency tables (`latency.nu`), `aether-log.nu summary --markdown` output, `fps.nu` output, observations. No addresses, host names or codes.

</details>
