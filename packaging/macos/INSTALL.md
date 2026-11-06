# AetherDesk host for macOS (alpha)

The Mac side of AetherDesk: it streams this Mac's screen to the Linux client
and takes its keyboard and mouse. Apple Silicon, macOS 14 or later.

## Install

Three ways, all ending with **AetherDesk.app** in `/Applications`, a daemon,
an agent per session, a menu-bar item, and an audio device, **AetherDesk**
(the Mac's sound pauses for a moment while it loads):

- **Installer:** open `AetherDesk-<version>.pkg` from the release and
  follow it. No Terminal needed. The app then updates itself (below).
- **Homebrew:** `brew install --cask stormlantern-dev/tap/aetherdesk-host`,
  then `aether-agent install` (as yourself, not with sudo; it asks for your
  password).
- **This tarball:** `AetherDesk.app/Contents/MacOS/aether-agent install`,
  from the unpacked folder.

Once, turn **AetherDesk** on in System Settings → Privacy & Security, under
**Screen & System Audio Recording** and under **Accessibility** (if it isn't
listed: + button, then pick AetherDesk in Applications). The menu-bar item
shows whether both are granted and opens those panes. macOS only applies
them to a fresh start, so then:

```sh
aether-agent restart
```

`aether-agent status` shows whether it runs; a missing permission shows in
its log.

Coming from an alpha that installed a bare `aether-agent` in
`/Library/Application Support/AetherDesk`: installing removes that copy
and keeps the pairings, but the permissions now belong to the app, so grant
them once more (the old `aether-agent` entries can go).

## Sound

During a session the Mac plays to the AetherDesk device, so its sound comes
out on Linux and the Mac stays quiet. Apps with their own speaker setting
(Teams, Zoom) can pick AetherDesk too. The Mac's volume keys do nothing
meanwhile; use the volume on Linux. With `aetherdesk --mic` on Linux,
AetherDesk is also the Mac's microphone for the session. Afterwards the Mac's
own devices are back.

## Pair a client

```sh
sudo aether-agent pair
```

shows a 6-digit code for 2 minutes; type it into `aetherdesk pair` on Linux.
`sudo aether-agent clients` lists paired clients, `sudo aether-agent unpair NAME`
forgets one.

## Updates

- **Installed with the `.pkg` (or from the tarball):** the app checks for
  updates once a day and shows the release notes when there is one; **Check
  for Updates…** in its menu checks now. Installing asks for an admin
  password, since the daemon runs as root. The update is signed (Sparkle,
  EdDSA) and the app refuses one that isn't.
- **Installed with Homebrew:** `brew upgrade`, then `aether-agent install`
  again: the installed copy is what runs. The app's own updater stays off,
  so the two don't fight.

Releases are signed with AetherDesk's Developer ID and notarized, so the two
permissions stay granted across updates.

## Remove

```sh
aether-agent uninstall
```

removes the app, the daemon, agents, audio device, logs and pairings. The
entries in Privacy & Security stay until you remove them there.
