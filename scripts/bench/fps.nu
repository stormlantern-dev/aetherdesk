#!/usr/bin/env nu
# Smoothness from a screen recording, the same way for every tool (#33,
# docs/benchmark.md): how many distinct pictures per second the client
# showed, and how long the picture stood still at worst.
#
#   scripts/bench/fps.nu record rustdesk-scroll          records 10 s of the monitor
#   scripts/bench/fps.nu record rustdesk-scroll --cpu rustdesk   … and the Mac's CPU meanwhile
#   scripts/bench/fps.nu rustdesk-scroll.mkv             analyses a recording
#   scripts/bench/fps.nu rustdesk-scroll.mkv --skip 1    ignore the first second
#
# ffmpeg's mpdecimate filter drops frames that look like the previous one; the
# frames it keeps are the distinct pictures. A stall is a gap between kept
# frames of more than 50 ms (3 refreshes at 60 Hz). Keep something moving the
# whole time (scrolling, a dragged window): a still screen counts as stalled.
#
# Recordings go to ~/.local/state/aetherdesk-bench/, outside the repo.

# Sensitive enough for a small dragged window (1 % of the blocks changing),
# while x264 noise on repeated frames stays below lo.
const DECIMATE = "mpdecimate=hi=768:lo=320:frac=0.01"

def main [
  video: string             # the recording
  --skip: number = 0        # seconds to ignore at the start
  --duration: number = 0    # seconds to analyse (0: all)
] {
  let file = if ($video | path exists) { $video } else { state-dir | path join $video }
  if not ($file | path exists) { error make {msg: $"no such recording: ($video)"} }

  mut args = ["-hide_banner" "-nostats" "-loglevel" "debug"]
  if $skip > 0 { $args = ($args | append ["-ss" ($skip | into string)]) }
  $args = ($args | append ["-i" $file])
  if $duration > 0 { $args = ($args | append ["-t" ($duration | into string)]) }
  $args = ($args | append ["-vf" $DECIMATE "-an" "-f" "null" "-"])

  let log = ^ffmpeg ...$args | complete | get stderr
  let frames = $log | lines | where {|l| $l =~ 'mpdecimate' and $l =~ ' (keep|drop) pts:' }
    | each {|l| $l | parse --regex ' (?<what>keep|drop) pts:\S+ pts_time:(?<t>[\d.]+)' | get -o 0 }
    | compact
    | each {|f| {keep: ($f.what == "keep"), t: ($f.t | into float)} }
  if ($frames | is-empty) { error make {msg: "ffmpeg reported no frames (is this a video?)"} }

  let start = $frames | first | get t
  let end = $frames | last | get t
  let seconds = $end - $start
  let kept = $frames | where keep | get t
  let gaps = $kept | append $end | window 2 | each {|w| ($w.1 - $w.0) * 1000 }
  let stalls = $gaps | where {|g| $g > 50 }
  let r = {|v| $v | math round --precision 1 }
  {
    recording: ($file | path basename)
    seconds: (do $r $seconds)
    "recorded fps": (do $r (($frames | length) / ($seconds + 0.000001)))
    "distinct fps": (do $r (($kept | length) / ($seconds + 0.000001)))
    "stalls > 50 ms": ($stalls | length)
    "longest stall ms": (if ($gaps | is-empty) { 0 } else { $gaps | math max | math round })
  }
}

# Records the monitor for a while with wf-recorder (pacman -S wf-recorder).
def "main record" [
  name: string               # file name, without extension
  --seconds (-s): int = 10   # how long
  --output (-o): string      # monitor (default: the focused one)
  --cpu: string              # also sample this tool's CPU on the Mac over SSH (see mac.nu cpu)
] {
  let dir = state-dir
  mkdir $dir
  let file = $dir | path join $"($name).mkv"
  let out = $output | default (^hyprctl monitors -j | from json | where focused | get 0.name)
  let cpu_file = $dir | path join $"($name).cpu.txt"
  rm -f $cpu_file
  if $cpu != null {
    let mac = $env.FILE_PWD | path join "mac.nu"
    job spawn { ^$mac cpu $cpu --seconds $seconds o+e> $cpu_file }
  }
  print $"recording ($out) for ($seconds) s to ($file)…"
  # Fast lossless-ish encode so the recorder doesn't steal from the client.
  ^timeout --signal INT $seconds wf-recorder -y -o $out -f $file -c libx264 -p preset=ultrafast -p crf=15
    | complete | ignore
  print (main $file --skip 0.5)
  if $cpu != null {
    # The Mac's top needs a moment longer than the recording.
    for _ in 1..20 {
      if ($cpu_file | path exists) and ((open --raw $cpu_file) =~ 'samples|error|exit') { break }
      sleep 500ms
    }
    print (open --raw $cpu_file | str trim)
  }
}

def state-dir [] {
  $env.HOME | path join ".local/state/aetherdesk-bench"
}
