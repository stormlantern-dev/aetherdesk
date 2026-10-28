# Installing the AetherDesk client

This tarball holds a prebuilt `aetherdesk` binary for Linux. It is built on
current Arch Linux and links against the system SDL3 and ffmpeg, so it runs on
Arch, CachyOS and other up-to-date Arch-based systems. On other distributions,
build from source or wait for the Flatpak.

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
install -Dm755 aetherdesk -t ~/.local/bin/
install -Dm644 dev.stormlantern.AetherDesk.desktop -t ~/.local/share/applications/
install -Dm644 icons/dev.stormlantern.AetherDesk.svg -t ~/.local/share/icons/hicolor/scalable/apps/
for s in 32 48 64 128 256; do
  install -Dm644 icons/dev.stormlantern.AetherDesk-$s.png ~/.local/share/icons/hicolor/${s}x${s}/apps/dev.stormlantern.AetherDesk.png
done
```

System-wide: the same commands with `sudo` and `/usr/local/bin`,
`/usr/local/share/applications` and `/usr/local/share/icons` instead.

## First run

Pair with your Mac once (run `scripts/mac.zsh pair` on the Mac for the code),
then connect:

```sh
aetherdesk pair <mac-ip>
aetherdesk
```

AetherDesk also appears in your app launcher. Your identity and paired Macs
live in `~/.config/aetherdesk/`.

## Uninstall

Remove the files installed above. `~/.config/aetherdesk/` is left alone;
delete it too to forget all pairings.
