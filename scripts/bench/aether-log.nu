#!/usr/bin/env nu
# AetherDesk's own numbers for the benchmark in #33 (docs/benchmark.md).
#
# The Linux client prints a line per second with the frames it showed
# ("60 fps shown") and every 5 s the median capture → present latency per
# stage ("latency ms (median of N frames): … total 38.2 | rtt 1.9"). This
# script stamps those lines with the wall-clock time and turns them into a
# table, split at every network change made with scripts/bench/netem.nu.
#
#   scripts/bench/aether-log.nu run aether                 start the client, log to aether.log
#   scripts/bench/aether-log.nu run aether -- <command…>   same, with another client command
#   scripts/bench/aether-log.nu summary aether             one row per netem phase
#   scripts/bench/aether-log.nu mark "scroll test"         start a new phase by hand
#
# Freezes are gaps of more than 1.5 s between "fps shown" lines. The client
# prints nothing when the Mac's screen doesn't change, so keep something
# moving on the Mac (the stopwatch) while measuring. Resolution is ~1 s.
#
# Logs live in ~/.local/state/aetherdesk-bench/, outside the repo. Host
# names and addresses are replaced by placeholders as lines are written.

def main [] {
  print (help main)
}

# Runs the client and logs its output with timestamps. Stop it with Ctrl+C
# or by closing the window.
def "main run" [
  tag: string        # name of the log, e.g. aether or aether-wifi
  ...cmd: string     # client command (default: cargo run --release -p aetherdesk-linux-client)
] {
  let dir = state-dir
  mkdir $dir
  let file = $dir | path join $"($tag).log"
  let cmd = if ($cmd | is-empty) {
    ["cargo" "run" "--release" "-p" "aetherdesk-linux-client"]
  } else { $cmd }
  cd ($env.FILE_PWD | path dirname | path dirname)
  print $"logging to ($file)"
  $"(stamp) # start: (redact ($cmd | str join ' '))" | save --append $file
  "\n" | save --append $file
  ^($cmd.0) ...($cmd | skip 1) o+e>| lines | each {|line|
    let out = $"(stamp) (redact $line)"
    print $out
    $"($out)\n" | save --append $file
  } | ignore
}

# Adds a marker that starts a new phase in the summary. netem.nu adds its
# own markers when it changes the network.
def "main mark" [text: string] {
  let dir = state-dir
  mkdir $dir
  $"(stamp) ($text)\n" | save --append ($dir | path join "markers.log")
  print $"marked: ($text)"
}

# One row per phase: fps, freezes, latency and what happened after the phase began.
def "main summary" [
  tag: string        # the log to read
  --markdown (-m)    # print a Markdown table, ready to paste
] {
  let dir = state-dir
  let lines = read-stamped ($dir | path join $"($tag).log")
  if ($lines | is-empty) { error make {msg: $"nothing logged under ($tag)"} }
  let first = $lines | first | get time
  let last = ($lines | last | get time) + 1ms
  let markers = read-stamped ($dir | path join "markers.log")
    | where {|m| $m.time > $first and $m.time < $last }

  let starts = [{time: $first, text: "start"}] | append $markers
  let rows = $starts | enumerate | each {|s|
    let begin = $s.item.time
    let end = $starts | get -o ($s.index + 1) | get -o time | default $last
    phase $s.item.text $begin $end ($lines | where {|l| $l.time >= $begin and $l.time < $end })
  }
  if $markdown { $rows | to md } else { $rows }
}

# --- helpers ---------------------------------------------------------------

def phase [name: string, begin: datetime, end: datetime, lines: list] {
  let fps_lines = $lines | where {|l| $l.text =~ '^\s*\d+ fps shown' }
  let fps = $fps_lines | each {|l| $l.text | parse --regex '(?<n>\d+) fps' | get n.0 | into int }
  let lat = $lines | where {|l| $l.text =~ 'latency ms' } | each {|l|
    $l.text | parse --regex 'total (?<total>[\d.]+).*rtt (?<rtt>[\d.]+)' | get -o 0
  } | compact
  # Gaps between pictures, counting the phase's start and end as edges.
  let times = [$begin] | append ($fps_lines | get time) | append $end
  let gaps = $times | window 2 | each {|w| ($w.1 - $w.0) / 1sec } | where {|g| $g > 1.5 }
  let first_picture = $fps_lines | get -o 0.time
  {
    phase: $name
    at: ($begin | format date "%H:%M:%S")
    seconds: (($end - $begin) / 1sec | math round)
    "first picture (s)": (if $first_picture == null { "none" } else { ($first_picture - $begin) / 1sec | math round --precision 1 })
    "median fps": (if ($fps | is-empty) { "–" } else { $fps | math median })
    freezes: ($gaps | length)
    "longest freeze (s)": (if ($gaps | is-empty) { 0 } else { $gaps | math max | math round --precision 1 })
    "capture→present ms": (median-of $lat total)
    "rtt ms": (median-of $lat rtt)
    reconnects: ($lines | where {|l| $l.text =~ '^connection lost' } | length)
    "gave up": ($lines | any {|l| $l.text =~ '^still no connection' })
  }
}

def median-of [rows: list, col: string] {
  if ($rows | is-empty) { "–" } else { $rows | get $col | into float | math median | math round --precision 1 }
}

def read-stamped [file: string] {
  if not ($file | path exists) { return [] }
  open --raw $file | lines | where {|l| not ($l | str contains " # start: ") } | each {|l|
    let p = $l | parse --regex '^(?<time>\S+) (?<text>.*)$' | get -o 0
    if $p == null { null } else {
      {time: ($p.time | into datetime --format "%Y-%m-%dT%H:%M:%S.%3f"), text: $p.text}
    }
  } | compact
}

def stamp [] {
  date now | format date "%Y-%m-%dT%H:%M:%S.%3f"
}

# The user's state dir, also under sudo (netem.nu writes markers as root).
def state-dir [] {
  let home = if ($env.SUDO_USER? | is-not-empty) {
    ^getent passwd $env.SUDO_USER | split row ":" | get 5
  } else { $env.HOME }
  $home | path join ".local/state/aetherdesk-bench"
}

# The repo is public: no host names or addresses in logs that may be pasted.
def redact [line: string] {
  $line
  | str replace --regex '^connecting to .*…$' 'connecting to <host> (<mac-ip>)…'
  | str replace --all --regex '\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b' '<mac-ip>'
  | str replace --all --regex '\[?\b[0-9a-fA-F]{1,4}(:[0-9a-fA-F]{0,4}){3,7}\]?(:\d+)?' '<mac-ip>'
}
