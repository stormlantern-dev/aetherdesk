# Installing the Auroval client

This tarball holds a prebuilt `auroval` binary for Linux. It is built on
current Arch Linux and links against the system SDL3 and ffmpeg, so it runs on
Arch, CachyOS and other up-to-date Arch-based systems. On other distributions,
use the Flatpak from the same release (`auroval-<version>-x86_64.flatpak`,
`flatpak install --user <file>`), which runs anywhere.

On Arch, the [PKGBUILD](https://github.com/stormlantern-dev/aetherdesk/tree/main/packaging/arch)
is the easier route: it builds from source and `pacman` tracks the files.

## Dependencies

Not bundled; install them with your package manager:

- SDL3 (`sdl3`)
- ffmpeg 9 (`ffmpeg`; the binary needs `libavcodec.so.63`)
- a Wayland compositor (the client is tested on Hyprland)

```sh
sudo pacman -S --needed sdl3 ffmpeg
```

## Install

For your user only (make sure `~/.local/bin` is on your `PATH`):

```sh
install -Dm755 auroval -t ~/.local/bin/
install -Dm644 nl.stormlantern.Auroval.desktop -t ~/.local/share/applications/
install -Dm644 icons/nl.stormlantern.Auroval.svg -t ~/.local/share/icons/hicolor/scalable/apps/
for s in 32 48 64 128 256; do
  install -Dm644 icons/nl.stormlantern.Auroval-$s.png ~/.local/share/icons/hicolor/${s}x${s}/apps/nl.stormlantern.Auroval.png
done
```

System-wide: the same commands with `sudo` and `/usr/local/bin`,
`/usr/local/share/applications` and `/usr/local/share/icons` instead.

## First run

Pair with your Mac once: run `sudo auroval-agent pair` on the Mac for the
code, then type it in here. `auroval pair` finds the Mac on the LAN; over a
VPN, give its address (`auroval pair <mac-ip>`). Then connect:

```sh
auroval pair
auroval
```

`auroval --mic` also sends this microphone to the Mac (it needs an ffmpeg
with libopus, as Arch's has). `auroval --help` lists the other options.

Auroval also appears in your app launcher. Your identity and paired Macs
live in `~/.config/auroval/`.

## Uninstall

Remove the files installed above. `~/.config/auroval/` is left alone;
delete it too to forget all pairings.
