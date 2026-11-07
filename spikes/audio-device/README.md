# Spike: a virtual Auroval audio device (#82)

The question: can a Core Audio Audio Server plug-in on the Mac hand what
apps play to our own process, fast enough to replace the ScreenCaptureKit
sound capture and the muting?

Three small C programs, built on the Mac with the Command Line Tools:

- `driver.c` + `Info.plist`: **Auroval.driver**, an output device
  "Auroval" (stereo float32, 48 kHz, host clock). On StartIO it looks up
  the Mach service `nl.stormlantern.auroval.audio`, which its Info.plist
  lists under `AudioServerPlugIn_MachServices`, gets a memory entry back and
  maps it. Each IO cycle's mix (WriteMix) goes into that ring buffer.
- `reader.c`: a root launchd daemon that owns the Mach service, allocates
  the ring (`shared.h`) and reads it every millisecond. It logs the level
  once a second and every click the probe plays, to
  `/tmp/auroval-audio-spike.log`.
- `probe.c`: plays ten clicks into the device, each with its own loudness,
  and matches them against the reader's log to get the delay.

```zsh
./build.zsh                # no sudo; signs ad hoc
sudo ./install.zsh         # restarts coreaudiod: sound drops for a moment
build/probe --clicks 10
sudo ./uninstall.zsh
```

## Results (MacBook Pro M3, macOS 27.2, 2026-11-03)

- The device shows up (System Settings, `system_profiler SPAudioDataType`)
  right after coreaudiod restarts. coreaudiod runs the plug-in in its own
  sandboxed helper (`Core-Audio-Driver-Service.helper`).
- **An ad-hoc signed plug-in loads.** AMFI logs that it's ad hoc, but
  coreaudiod loads it anyway. Developer ID signing is needed for releases,
  not for development.
- **The sandbox lets the plug-in reach our daemon** through the Mach service
  listed in Info.plist, and mapping the memory entry the daemon sends works.
  No other IPC was needed.
- **Latency**, 10 of 10 clicks seen, 512-frame IO buffer:
  - from the app rendering a buffer to the reader seeing it: min 0.07 ms,
    median 1.72 ms, max 4.27 ms. Most of that is the reader's 1 ms polling.
  - compared with a real speaker, the reader has the sound 8–19 ms *before*
    it would play: Core Audio hands the mix over ahead of its output time.
- **Real apps:** YouTube in a browser and a Teams test call, both with
  Auroval as the output, reached the reader at normal levels (peaks
  0.4–0.8).
- When idle (only silence), coreaudiod slows the cycle down (about 11 per
  second instead of 94); the reader copes.

## Still open

- The login window: whether coreaudiod serves the plug-in there (the reader
  runs as a system daemon, so the service is there).
- The reader restarting while IO runs: the plug-in only reconnects on the
  next StartIO.
- The microphone direction (an input stream, same ring idea reversed).
