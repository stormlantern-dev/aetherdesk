# L0 spikes: Hyprland host

Spikes for the [Linux host → macOS client](../../docs/PRD-linux-host.md) direction, tracked in #63.

## S5: capture + encode latency

`l0` captures one output with `ext-image-copy-capture` into shared memory and encodes every frame with NVENC (through ffmpeg), timing each stage per frame: compositor copy, upload into an ffmpeg frame, CPU conversion to 4:4:4 (only for `hevc444`), and encode.

Build in the devcontainer, run on the host (NVENC needs the host's GPU; host and container both have ffmpeg 9):

```nu
devcontainer exec --docker-path podman --workspace-folder . bash -lc 'cd spikes/l0 && CARGO_TARGET_DIR=target cargo build --release'
```

Run it against a compositor with something moving on the output (capture waits for damage):

```nu
with-env { WAYLAND_DISPLAY: wayland-2 } { spikes/l0/target/release/l0 --output WAYLAND-1 --frames 300 --codec hevc }
```

`--codec` is `hevc`, `hevc444` or `h264`; `--mbit` sets the bit rate (default 20).

To measure without touching your own session, run a second Hyprland nested in a window with its own config (`HYPRLAND_NO_SD_VARS=1` keeps it out of the systemd user environment), set its `WAYLAND-1` output to the size you want, and play a test pattern full-screen on it (`ffplay -f lavfi -i testsrc2=s=2560x1440:r=60 -fs`). A Hyprland with only a headless backend doesn't start: it needs a DRM session or a parent compositor for its GPU allocator.
