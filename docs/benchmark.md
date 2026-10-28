# Benchmark run sheet

How to run the comparison from #33 in one afternoon: AetherDesk against Pippin, Apple Screen Sharing (VNC) and RustDesk, optionally Sunshine/Moonlight and Parsec. Every measurement is one command or one photo, and every result goes into one table in [`scripts/bench/results-template.md`](../scripts/bench/results-template.md), which is pasted into #33 at the end.

Machines: the Mac (M3, macOS Tahoe, 3456×2234) and the Linux client (CachyOS, Hyprland, NVIDIA, 2560×1440 at 60 Hz), on the same LAN as always. Note whether the Mac is on Wi-Fi or Ethernet; it stays that way all afternoon.

**The repo is public.** No IP addresses, pairing codes, passwords or host names in anything you paste. The scripts use placeholders (`<mac-ip>`, `<host>`) and keep their logs and recordings outside the repo, in `~/.local/state/aetherdesk-bench/`.

## Helpers

All in `scripts/bench/`, nushell, run from the repo root.

| Script | Where | What |
|---|---|---|
| `netem.nu` | Linux | Packet loss, delay/jitter and a 10-s cut on the traffic with the Mac only. `setup` (once), `apply`, `cut`, `clear`, `show`. |
| `latency.nu` | anywhere | Stopwatch photos → table and median, like #7. |
| `fps.nu` | Linux | Records the monitor (`fps.nu record <name>`) and reports distinct pictures per second and the longest stall. Same method for every tool. |
| `aether-log.nu` | Linux | Runs the AetherDesk client and logs its own numbers (fps, capture → present, rtt, reconnects); `summary` splits them per network setting. |
| `mac-cpu.nu` | Mac | Average and peak CPU of a tool's processes during the scroll test, plus WindowServer. |
| `mac.nu` | Linux | Drives the Mac over SSH: `check` (reachable, repo, clock offset), `motion`, `cpu`, `lock`, `run`. |
| `motion.html` | Mac | The moving picture every tool streams: a clock with milliseconds and a page that scrolls by itself (`#scroll`), a box that moves like a dragged window (`#move`), or only the clock (`#clock`). |

`netem.nu` only touches packets to and from the Mac (`$env.BENCH_MAC` or `--mac`), both directions by default: outgoing through a prio qdisc, incoming through an `ifb` device. The video comes in, so that's where loss hurts. The rest of the machine's network stays normal, including Claude's own connection when Claude runs the tests. Each change is also written to `~/.local/state/aetherdesk-bench/markers.log`, so the AetherDesk log splits itself per setting.

### One-time setup to run everything from Linux

So that one person, or Claude, can run the whole benchmark from the Linux machine. Nothing here goes into the repo; the Mac's address and user name stay in your shell environment and `~/.ssh/config`.

1. **The Mac's address**, in nushell's `env.nu` on Linux: `$env.BENCH_MAC = "<mac-ip>"`.
2. **Network rights without sudo:** `scripts/bench/netem.nu setup` (on Linux) prints the `sudo` lines still needed. One gives a *copy* of `tc` in `~/.local/lib/aetherdesk-bench/` the `cap_net_admin` capability (the system binary stays as it is); that lasts. The other creates the `ifb` device for incoming traffic; that is gone after a reboot, so run `setup` again after one. (A capable copy of `ip` doesn't work: `ip` drops capabilities set on its binary.) After that, `netem.nu` runs as your user. The same output shows how to undo it.
3. **SSH to the Mac with a key:** on the Mac, System Settings → General → Sharing → *Remote Login* on, only for your user. On Linux:
   ```nu
   ssh-keygen -t ed25519 -f ~/.ssh/aether-mac -C aetherdesk-bench
   ssh-copy-id -i ~/.ssh/aether-mac.pub <mac-user>@<mac-ip>
   ```
   and in `~/.ssh/config`:
   ```
   Host aether-mac
     HostName <mac-ip>
     User <mac-user>
     IdentityFile ~/.ssh/aether-mac
   ```
   On the Mac, limit that key to the Linux machine: in `~/.ssh/authorized_keys`, put `from="<linux-ip>" ` in front of the line that ends in `aetherdesk-bench`. The key has no passphrase (so scripts can use it), and this way it only works from that one address.

   If the repo on the Mac isn't in `~/Documents/projects/aetherdesk`, set `$env.BENCH_MAC_REPO`. Then `scripts/bench/mac.nu check` should print the macOS version, the repo's last commit and the clock offset.
4. **Saved credentials per tool,** so a tool can be started without anyone typing a password: Pippin once with `--save-password` (keyring), Remmina's profile with the password saved, RustDesk with the permanent password remembered, Parsec signed in. Claude never types passwords; these let it start the tools anyway.
5. **Clocks:** both machines on NTP (the default). `mac.nu check` shows the offset; under ~5 ms is fine.

### What's reachable, and from where

All of this is meant for the LAN only; nothing in the benchmark needs a port open on the router.

| Opened for the benchmark | Port | Who can get in | Off again |
|---|---|---|---|
| AetherDesk (installed host) | UDP 4500 | paired clients only (mutual TLS) | stays, as before |
| Screen Sharing (Pippin, VNC) | TCP/UDP 5900–5901 | your macOS user, with its password | Sharing → *Screen Sharing* |
| Remote Login (SSH) | TCP 22 | your macOS user: the bench key (from the Linux address only) or its password | Sharing → *Remote Login* |
| RustDesk | TCP 21118 (direct IP) | the whitelisted Linux address, with the permanent password | quit and remove the app |
| Sunshine (optional) | 47984–48010, web UI 47990 | paired Moonlight clients; web UI with admin password | `brew uninstall sunshine` |
| Parsec (optional) | via Parsec's servers | your Parsec account | *Hosting* off |

Before the day, check the router: no port forwards to the Mac, and UPnP off (or at least no entries for these ports in its UPnP table). On Linux, the capable `tc` copy lets any of your own processes change network settings, and the SSH key has no passphrase: both only matter if something already runs as you, but remove them after the benchmark ("Undo everything" below).

## Before the day: install

### Linux (CachyOS)

```nu
sudo pacman -S --needed remmina libvncserver freerdp tigervnc moonlight-qt wf-recorder ffmpeg libfdk-aac
paru -S rustdesk-bin            # AUR; or yay
paru -S parsec-bin              # optional, AUR
```

- **AetherDesk:** current `main`, built as usual (`cargo build --release -p aetherdesk-linux-client`). Paired with the installed host (`aetherdesk pair <mac-ip>`, code from `scripts/mac.zsh pair` on the Mac).
- **Pippin:** not in the AUR yet (its README: packages follow when AUR registration reopens). Build from the PKGBUILD in its repo:
  ```nu
  git clone https://github.com/jonesy827/pippin ~/src/pippin
  cd ~/src/pippin/packaging/aur/pippin-git; makepkg -si
  ```
  Or `cargo install --locked --git https://github.com/jonesy827/pippin` (Rust ≥ 1.95, needs clang and ffmpeg headers). `libfdk-aac` is only for audio. On NVIDIA it decodes with CUDA (Turing or newer). Its README suggests a larger UDP buffer for high resolutions: `sudo sysctl -w net.core.rmem_max=8388608` (record it as a non-default setting if you use it).
- **Apple Screen Sharing (VNC):** Remmina with its VNC plugin (`libvncserver` provides it). TigerVNC's `vncviewer` is the fallback, but it can't scale to Fit, so Remmina is the one in the table.
- **RustDesk:** `rustdesk-bin` from the AUR (1.5.x at the time of writing; `rustdesk-appimage` also exists). Not in the official repos.
- **Sunshine/Moonlight (optional):** `moonlight-qt` is in `extra`.
- **Parsec (optional):** the macOS app, *Hosting* enabled in its settings. Parsec always connects through its own servers, so the account is the lock: two-factor on, no friends or guests given access, *Hosting* off again after the test. If hosting doesn't work on Tahoe, note it and skip.

Check that `wf-recorder -o <monitor> -f /tmp/x.mkv` records on Hyprland (Ctrl+C after a second) before the day.

### Mac

- **AetherDesk:** installed host from `main`: `scripts/mac.zsh update main`, then `scripts/mac.zsh install` (sudo, you run it). `scripts/mac.zsh status` shows daemon and agent running.
- **Screen Sharing** (for Pippin and VNC): System Settings → General → Sharing → *Screen Sharing* on, ⓘ → *Allow access for: Only these users* with just your user. Log in from Remmina with your macOS user (Apple's own authentication, which Remmina supports). Leave *VNC viewers may control screen with password* **off**: that is the old VNC password (8 characters, no encryption of the session). Pippin needs Apple Silicon and macOS 14+, UDP 5900–5901 open.
- **RustDesk:** `brew install --cask rustdesk` (or the `.dmg` from GitHub releases). Grant *Screen & System Audio Recording* and *Accessibility*. **Keep it off the internet:** by default RustDesk registers the Mac with RustDesk's public servers, and then anyone with its ID and the permanent password can connect from anywhere. So in Settings → Network → *ID/Relay server*, set the ID server to `127.0.0.1` (no public registration; direct IP still works). Settings → Security: *Enable direct IP access* (port 21118), *IP whitelisting* with only the Linux machine's address, and a long random permanent password. Quit RustDesk between test sessions.
- **Sunshine (optional):** `brew tap LizardByte/homebrew; brew install sunshine`, then pair Moonlight through its web UI. In its settings keep *UPnP* **off** (it would open ports on the router) and the web UI to LAN only (the default), with a strong admin password. macOS support is experimental; if it doesn't run on Tahoe, write that down and skip.
- **Parsec (optional):** the macOS app, *Hosting* enabled in its settings. Same: if hosting doesn't work on Tahoe, note it and skip.
- **The motion page** (`scripts/bench/motion.html`, in the repo on the Mac): `scripts/bench/mac.nu motion clock` opens it in Safari; put Safari full screen (Ctrl+Cmd+F) once. Its clock is the stopwatch for the photos, and its scroll and move modes are the moving picture for the smoothness and loss tests, the same for every tool.
- **A terminal at 11 pt** with some text (e.g. `ls -l /usr/bin | head -60`).

Write down the version of every tool as you install it; the results template has a table for that.

## Same conditions for every tool

- **Window:** the client is the only window on an empty Hyprland workspace, made fullscreen with Hyprland's own fullscreen (so every tool gets 2560×1440, whatever it supports itself). With AetherDesk, release the keyboard grab (Right Ctrl) first.
- **Scaling: Fit**, keeping the aspect ratio (the Mac's 3456×2234 letterboxed into 2560×1440). Remmina: *scaled mode*, aspect kept. RustDesk: *Scale adaptive*. Pippin: start with `--no-auto-resize`, otherwise it changes the Mac's resolution to the window and streams far fewer pixels; one extra run with auto-resize is interesting but goes in its own row.
- **Mac resolution:** default (3456×2234 backing, "Default" in Displays). No other remote-desktop tool connected at the same time; quit the others.
- **Quality settings:** defaults, unless a tool can't work without a change. Every change goes in the "non-default settings" table.
- **Network:** `scripts/bench/netem.nu show` says no netem before each tool.
- **Motion:** the motion page full screen on the Mac, nothing else moving there.

## Test order, per tool

Do all tests for one tool, then the next. AetherDesk first, so the method is settled on the tool we know. Roughly 30–40 minutes per tool.

### 1. Connect

| Tool | Connect from Linux |
|---|---|
| AetherDesk | `scripts/bench/aether-log.nu run aether` (starts `cargo run --release -p aetherdesk-linux-client` and logs; pass `-- <command…>` for another command, e.g. in the devcontainer) |
| Pippin | `pippin <mac-ip> -u <mac-user> --no-auto-resize --stats-json ~/.local/state/aetherdesk-bench/pippin.jsonl` (password from the keyring or prompt; never on the command line in a pasted log) |
| Apple VNC | Remmina → new VNC profile, server `<mac-ip>`, colour depth *true colour (24 bit)*, quality as default, scaled mode on |
| RustDesk | type `<mac-ip>` in the *Control remote desktop* field, permanent password |
| Moonlight | add host by IP, pair via Sunshine's web UI, stream *Desktop* at 2560×1440 60 fps |
| Parsec | sign in on both, connect to the Mac from the computer list |

Record the version and any non-default setting now.

### 2. Latency (≥ 5 photos)

`scripts/bench/mac.nu motion clock`: the clock runs on the Mac. Take one photo with the phone that shows the Mac screen and the client screen side by side. At least 5, a few seconds apart. Read both times off each photo, then:

```nu
"3.010 2.945
5.121 5.056
7.138 7.106 client digits ghosted" | scripts/bench/latency.nu --tool AetherDesk
```

The first number on a line is the Mac, the second the client; words after them become a note. Paste the median into the table, the per-photo table into the raw notes.

For AetherDesk, also note the client's own capture → present number: `scripts/bench/aether-log.nu summary aether` (column *capture→present ms*). It covers capture on the Mac to `present` here; the stopwatch adds the Mac's compositor and both displays (in #7: ~38 ms of ~65 ms).

### 3. Smoothness (scroll, window drag) and Mac CPU

Two recordings of 10 s each, all from Linux. The motion page does the scrolling and moving on the Mac, so the picture is the same for every tool and nobody's hand is in the measurement:

```nu
scripts/bench/mac.nu motion scroll
scripts/bench/fps.nu record aether-scroll --cpu aether   # cpu: aether, screensharing, rustdesk, sunshine, parsec
scripts/bench/mac.nu motion move                   # stands in for dragging a window
scripts/bench/fps.nu record aether-drag
```

`--cpu` samples the tool's processes on the Mac over SSH during the same 10 s. (Without SSH: run `mac-cpu.nu` on the Mac itself and scroll or drag by hand.) `fps.nu` reports *distinct fps* (pictures that differ from the one before) and the *longest stall*. Also note stutter or tearing you see. The tool's own fps counter, where it has one, goes in the notes: AetherDesk logs it, Pippin writes `presented_fps` to its stats file, RustDesk has *Show quality monitor*, Moonlight Ctrl+Alt+Shift+S.

`mac-cpu.nu` uses `top`; for Pippin and VNC the Mac side is Apple's `screensharingd` and friends. 100 % = one core. If its numbers look off, Activity Monitor's % CPU for the same processes is the fallback.

### 4. Picture quality

Terminal at 11 pt on the Mac, full-screen client. Take a screenshot of the client on Linux (`grim -g (slurp) <file>.png` or Hyprland's screenshot binding), cropped to the same text area for every tool. Look for blur and colour fringes around red/blue text (chroma subsampling, 4:2:0 vs 4:4:4). The crops go side by side in the raw notes or a gist.

### 5. Packet loss and jitter

`scripts/bench/mac.nu motion scroll`, so the picture always changes. For each setting, apply it (or run the `sudo` line it prints, without `setup`), then watch for **60 s**:

```nu
scripts/bench/netem.nu apply --loss 1
scripts/bench/netem.nu apply --loss 3
scripts/bench/netem.nu apply --loss 5
scripts/bench/netem.nu apply --delay 30 --jitter 10
scripts/bench/netem.nu clear
```

Per setting, note: does the picture freeze, how often and how long at worst, does it recover on its own, does input still arrive (type a word in TextEdit, move a window). A quick `scripts/bench/fps.nu record <tool>-loss3 --seconds 20` while scrolling gives the stall numbers in the same terms as test 3.

Tool numbers where they exist:

- AetherDesk: `scripts/bench/aether-log.nu summary aether --markdown`, one row per netem phase (freezes, longest freeze, reconnects).
- Pippin: per-second `frozen_ms`, `presented_fps` and recovery times in its stats file:
  ```nu
  let s = open --raw ~/.local/state/aetherdesk-bench/pippin.jsonl | from json --objects
  {fps: ($s.presented_fps | compact | math median), frozen_s: ($s.frozen_ms | compact | math sum | $in / 1000)}
  ```
  (Start a new file per setting, or select the seconds by time.) Its default after loss is `--after-loss idr` (freeze until the next keyframe). One extra run at 1 % and 3 % with `--after-loss conceal` is fair to include as its own row, marked non-default.

### 6. 10-s interruption

```nu
scripts/bench/netem.nu cut --seconds 10
```

It drops all traffic for 10 s, restores it, and prints both times. Measure from *restored* to a usable picture (stopwatch on the Mac moving again and input working), and note whether you had to do anything (reconnect, click a dialog). For AetherDesk, the `restored` row in `aether-log.nu summary aether` shows *first picture (s)*.

### 7. Login window and locked Mac

- **Locked:** `scripts/bench/mac.nu lock` (or Ctrl+Cmd+Q on the Mac), then connect fresh with the client: can you see the lock screen and type the password?
- **Login window:** Apple menu → Log Out, then connect fresh: can you see the login window and log in?

Per tool: yes / no / only with a setting (which).

### 8. Wrap up the tool

`scripts/bench/netem.nu clear`, quit the client, quit the tool on the Mac. Next tool.

## Afterwards

Fill in `scripts/bench/results-template.md` (copy it to `~/.local/state/aetherdesk-bench/results.md` first, so the template stays blank), then post it:

```nu
open ~/.local/state/aetherdesk-bench/results.md | gh issue comment 33 --repo stormlantern-dev/aetherdesk --body-file -
```

Read it once more for addresses, host names and codes before posting. Photos and screenshots go in a gist or the comment. Every place where AetherDesk is behind becomes its own issue.

## Undo everything

Everything the setup and the run change, per machine, so it can all be put back. Keep the results (`~/.local/state/aetherdesk-bench/`) until they're posted.

**On Linux** (nushell):

```nu
scripts/bench/netem.nu clear                                     # if a netem is still active
~/src/pippin/target/release/pippin <mac-ip> -u <mac-user> --forget-password   # Pippin's keyring entry
sudo rm -r ~/.local/lib/aetherdesk-bench                         # the capable copy of tc
sudo ip link del aebench0                                        # the ifb device (also gone after a reboot)
rm ~/.config/nushell/autoload/aetherdesk-bench.nu                # BENCH_MAC
rm ~/.ssh/aether-mac ~/.ssh/aether-mac.pub                       # the SSH key for the Mac
rm -r ~/src/pippin                                               # Pippin build
rm -r ~/.local/state/aetherdesk-bench                            # logs, markers, recordings, results
flatpak uninstall --user --delete-data org.remmina.Remmina com.moonlight_stream.Moonlight com.rustdesk.RustDesk com.parsecgaming.parsec
sudo pacman -Rns wf-recorder
```

Then remove the `Host aether-mac` block from `~/.ssh/config` by hand.

**On the Mac** (any shell):

```zsh
sed -i '' '/ aetherdesk-bench$/d' ~/.ssh/authorized_keys          # the Linux key
rm -f ~/.ssh/authorized_keys.bak-aetherdesk                       # backup from adding from=
scripts/mac.zsh update main                                      # repo back on main
```

And in System Settings → General → Sharing: *Remote Login* off, *Screen Sharing* off if it was off before. RustDesk: quit it, move the app to the Bin, and remove it from Privacy & Security → *Screen & System Audio Recording* and *Accessibility*. Sunshine (`brew uninstall sunshine`) and Parsec, if installed.

## Limits of the method

- Stopwatch photos have ±1 refresh (~17 ms) on each screen plus camera exposure; that's why the median of ≥ 5.
- `fps.nu` sees what the Linux compositor showed at 60 Hz, so it can't report more than 60. Small changes (a single changing digit) can be missed as "same picture"; scrolling and dragging are large enough.
- `aether-log.nu` freezes have ~1 s resolution (the client prints fps once a second).
- `netem.nu` shapes the client side only. Loss on Wi-Fi in the wild is burstier than netem's independent random loss.
