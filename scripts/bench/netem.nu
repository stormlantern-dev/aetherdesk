#!/usr/bin/env nu
# Packet loss, delay and jitter on the Linux client's network interface, for
# the benchmark in #33 (docs/benchmark.md).
#
#   scripts/bench/netem.nu apply --loss 1                 1 % loss, both directions
#   scripts/bench/netem.nu apply --delay 30 --jitter 10   30 ms ± 10 ms
#   scripts/bench/netem.nu cut --seconds 10               drop everything for 10 s
#   scripts/bench/netem.nu clear                          back to normal
#   scripts/bench/netem.nu show                           what is active now
#
# tc needs root. Run as a normal user, the script only prints one
# `sudo sh -c '…'` line to copy and run; it never calls sudo itself. Run it
# as root (`sudo nu scripts/bench/netem.nu apply …`) and it applies directly.
#
# The interface: give it as the first argument, or pass --mac <address> (or
# set $env.BENCH_MAC) to use the interface towards the Mac, or leave both out
# for the interface of the default route. The address is only used locally.
#
# Each change is also written to ~/.local/state/aetherdesk-bench/markers.log,
# so scripts/bench/aether-log.nu can split the AetherDesk log per setting.
#
# Direction: video flows Mac → Linux, so loss on the way in is what freezes
# a picture. A root qdisc only shapes what leaves the interface; for incoming
# traffic the script redirects it through an ifb device and shapes that.
# The default, both, impairs both directions (input and acks too), like a
# bad Wi-Fi link would. Everything on the interface is affected, not only the
# remote-desktop traffic: keep downloads off while testing.

const IFB = "ifb0"

def main [] {
  print (help main)
}

# Adds loss and/or delay. With nothing given it clears instead.
def "main apply" [
  iface?: string          # network interface (default: towards --mac, or the default route)
  --mac: string           # the Mac's address, to find the interface
  --loss: number = 0      # packet loss in percent
  --delay: int = 0        # added delay in ms
  --jitter: int = 0       # delay variation in ms (needs --delay)
  --direction: string = "both"  # in, out or both
] {
  let dev = (pick-iface $iface $mac)
  let spec = (netem-spec $loss $delay $jitter)
  if ($spec | is-empty) {
    run-as-root ((clear-cmds $dev) | append (mark "netem off")) $"no impairment on ($dev)"
    return
  }
  let cmds = [
    (clear-cmds $dev)
    (add-cmds $dev $spec $direction)
    (mark $"netem ($spec) \(($direction))")
  ] | flatten
  run-as-root $cmds $"netem ($spec) on ($dev), direction ($direction)"
}

# Removes everything this script added.
def "main clear" [
  iface?: string
  --mac: string
] {
  let dev = (pick-iface $iface $mac)
  run-as-root ((clear-cmds $dev) | append (mark "netem off")) $"no impairment on ($dev)"
}

# Drops all traffic for a while, then restores it. Prints the wall-clock time
# of the cut and the restore (to compare with scripts/bench/aether-log.nu).
def "main cut" [
  iface?: string
  --mac: string
  --seconds: int = 10     # how long the network is gone
] {
  let dev = (pick-iface $iface $mac)
  let cmds = [
    (clear-cmds $dev)
    (add-cmds $dev "loss 100%" "both")
    (mark $"cut ($seconds) s")
    'echo "cut      $(date +%H:%M:%S.%3N)"'
    $"sleep ($seconds)"
    (clear-cmds $dev)
    (mark "restored")
    'echo "restored $(date +%H:%M:%S.%3N)"'
  ] | flatten
  run-as-root $cmds $"network on ($dev) cut for ($seconds) s"
}

# Shows the active qdiscs. Needs no root.
def "main show" [
  iface?: string
  --mac: string
] {
  let dev = (pick-iface $iface $mac)
  print $"--- ($dev)"
  ^tc qdisc show dev $dev
  if (^ip link show $IFB | complete | get exit_code) == 0 {
    print $"--- ($IFB) \(incoming traffic of ($dev))"
    ^tc qdisc show dev $IFB
  }
}

# --- helpers ---------------------------------------------------------------

def pick-iface [iface?: string, mac?: string] {
  if $iface != null { return $iface }
  let target = $mac | default ($env.BENCH_MAC? | default "1.1.1.1")
  let route = ^ip route get $target | str trim
  let dev = $route | parse --regex 'dev (?<dev>\S+)' | get dev.0?
  if $dev == null {
    error make {msg: "can't find the interface: give it as an argument"}
  }
  $dev
}

# "loss 1% delay 30ms 10ms", or "" when there is nothing to do.
def netem-spec [loss: number, delay: int, jitter: int] {
  if $jitter > 0 and $delay == 0 {
    error make {msg: "--jitter needs --delay (netem varies an existing delay)"}
  }
  mut parts = []
  if $loss > 0 { $parts = ($parts | append $"loss ($loss)%") }
  if $delay > 0 {
    let j = if $jitter > 0 { $" ($jitter)ms distribution normal" } else { "" }
    $parts = ($parts | append $"delay ($delay)ms($j)")
  }
  $parts | str join " "
}

def add-cmds [dev: string, spec: string, direction: string] {
  if $direction not-in ["in" "out" "both"] {
    error make {msg: $"--direction must be in, out or both, not ($direction)"}
  }
  let out = if $direction in ["out" "both"] {
    [$"tc qdisc add dev ($dev) root netem ($spec)"]
  } else { [] }
  let inc = if $direction in ["in" "both"] {
    [
      "modprobe ifb numifbs=0"
      $"ip link add ($IFB) type ifb"
      $"ip link set ($IFB) up"
      $"tc qdisc add dev ($dev) handle ffff: ingress"
      $"tc filter add dev ($dev) parent ffff: matchall action mirred egress redirect dev ($IFB)"
      $"tc qdisc add dev ($IFB) root netem ($spec)"
    ]
  } else { [] }
  $out | append $inc
}

# Removing what isn't there fails harmlessly; errors are hidden.
def clear-cmds [dev: string] {
  [
    $"tc qdisc del dev ($dev) root 2>/dev/null"
    $"tc qdisc del dev ($dev) ingress 2>/dev/null"
    $"ip link del ($IFB) 2>/dev/null"
  ]
}

# A shell command that adds a phase marker for scripts/bench/aether-log.nu.
# The file is created here, as the user, so root only appends to it.
def mark [text: string] {
  let home = if ($env.SUDO_USER? | is-not-empty) {
    ^getent passwd $env.SUDO_USER | split row ":" | get 5
  } else { $env.HOME }
  let dir = $home | path join ".local/state/aetherdesk-bench"
  let file = $dir | path join "markers.log"
  if (^id -u | str trim) != "0" {
    mkdir $dir
    touch $file
  }
  $"echo \"$\(date +%Y-%m-%dT%H:%M:%S.%3N) ($text)\" >> ($file)"
}

# Runs the commands when we are root, else prints them as one sudo line.
def run-as-root [cmds: list<string>, what: string] {
  # `;` and not `&&`: the clear steps may fail when there's nothing to clear.
  let script = $cmds | str join "; "
  if (^id -u | str trim) == "0" {
    ^sh -c $script
    print $"done: ($what)"
  } else {
    print $"# ($what) — run this \(needs root):"
    print $"sudo sh -c '($script)'"
  }
}
