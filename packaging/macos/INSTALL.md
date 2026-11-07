# Auroval host for macOS (alpha)

The Mac side of Auroval: it streams this Mac's screen to the Linux client
and takes its keyboard and mouse. Apple Silicon, macOS 14 or later.

## Install

With Homebrew: `brew install --cask stormlantern-dev/tap/auroval-host`.
From this tarball: put `auroval-agent` somewhere on your `PATH`.

Then, as yourself (not with sudo; it asks for your password):

```sh
auroval-agent install
```

This copies the binary to `/Library/Application Support/Auroval`, starts
a daemon and an agent per session, and adds an audio device, **Auroval**
(the Mac's sound pauses for a moment while it loads).

Once, grant that copy **Screen Recording** and **Accessibility** in System
Settings → Privacy & Security (+ button, then Cmd+Shift+G and paste
`/Library/Application Support/Auroval/auroval-agent`). macOS only applies
them to a fresh start, so then:

```sh
auroval-agent restart
```

`auroval-agent status` shows whether it runs; a missing permission shows in
its log.

## Sound

During a session the Mac plays to the Auroval device, so its sound comes
out on Linux and the Mac stays quiet. Apps with their own speaker setting
(Teams, Zoom) can pick Auroval too. The Mac's volume keys do nothing
meanwhile; use the volume on Linux. With `auroval --mic` on Linux,
Auroval is also the Mac's microphone for the session. Afterwards the Mac's
own devices are back.

## Pair a client

```sh
sudo auroval-agent pair
```

shows a 6-digit code for 2 minutes; type it into `auroval pair` on Linux.
`sudo auroval-agent clients` lists paired clients, `sudo auroval-agent unpair NAME`
forgets one.

## Updates

After `brew upgrade`, run `auroval-agent install` again: the installed copy is
what runs. Releases are signed with Auroval's Developer ID and notarized,
so the two permissions stay granted across updates. (Coming from an older,
ad hoc signed alpha, macOS asks for them once more.)

## Remove

```sh
auroval-agent uninstall
```

removes the daemon, agents, audio device, logs and pairings. The entries in Privacy &
Security stay until you remove them there.
