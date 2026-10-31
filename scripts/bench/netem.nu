#!/usr/bin/env nu
# Packet loss, delay and jitter between this Linux client and the Mac, for
# the benchmark in #33 (docs/benchmark.md). Only traffic to and from the Mac
# is touched; the rest of the machine's network stays normal.
#
#   scripts/bench/netem.nu setup                          once: prints what to run as root
#   scripts/bench/netem.nu apply --loss 1                 1 % loss, both directions
#   scripts/bench/netem.nu apply --delay 30 --jitter 10   30 ms ± 10 ms
#   scripts/bench/netem.nu cut --seconds 10               drop everything for 10 s
#   scripts/bench/netem.nu clear                          back to normal
#   scripts/bench/netem.nu show                           what is active now
#
# The Mac's IPv4 address comes from --mac or $env.BENCH_MAC; it is used
# locally only, never written to the repo. The interface towards it is
# found with `ip route get`, or give it with --iface.
#
# Rights: `setup` prints two root lines. One gives a copy of tc (not the
# system one) the CAP_NET_ADMIN capability, in ~/.local/lib/aetherdesk-bench/;
# that lasts. The other creates the ifb device for incoming traffic; that
# lasts until a reboot. (A copy of ip can't do it: ip drops capabilities set
# on its binary, and systemd loads ifb without devices.) After that everything
# runs as the normal user. Without them the script prints one `sudo sh -c '…'`
# line per change instead; it never calls sudo itself.
#
# Direction: video flows Mac → Linux, so loss on the way in is what freezes a
# picture. Outgoing packets to the Mac go through a prio qdisc whose fourth
# band has the netem; incoming packets from the Mac are redirected through
# an ifb device with the netem. The default, both, impairs both directions
# (input and acks too), like a bad Wi-Fi link would.
#
# Each change is also written to ~/.local/state/aetherdesk-bench/markers.log,
# so scripts/bench/aether-log.nu can split the AetherDesk log per setting.

# Our own ifb device, so it can't clash with ifb devices of other tools.
const IFB = "aebench0"
const CAPS_DIR = ".local/lib/aetherdesk-bench"

def main [] {
  print (help main)
}

# Prints the root commands that let this script run without sudo.
def "main setup" [] {
  let dir = home | path join $CAPS_DIR
  if (caps-ready) and (ifb-ready) {
    print $"ready: ($dir)/tc has cap_net_admin and ($IFB) is up"
    return
  }
  if not (caps-ready) {
    # Made here, as the user: `install -d` as root would leave root-owned
    # parents such as ~/.local/lib behind.
    mkdir $dir
    print "# Once, as root: a copy of tc (not the system one) that may change network settings."
    print $"sudo sh -c 'install -o root -m 0755 /usr/bin/tc ($dir)/tc && setcap cap_net_admin+ep ($dir)/tc'"
    print ""
  }
  if not (ifb-ready) {
    print "# As root, again after every reboot: the device that incoming traffic is shaped on."
    print $"sudo sh -c 'ip link add ($IFB) type ifb && ip link set ($IFB) up'"
    print ""
  }
  print "# To undo everything:"
  print $"sudo sh -c 'rm -r ($dir); ip link del ($IFB)'"
}

# Adds loss and/or delay on the traffic with the Mac. With nothing given it clears.
def "main apply" [
  --mac: string           # the Mac's IPv4 address (default: $env.BENCH_MAC)
  --iface: string         # network interface (default: the one towards the Mac)
  --loss: number = 0      # packet loss in percent
  --delay: int = 0        # added delay in ms
  --jitter: int = 0       # delay variation in ms (needs --delay)
  --direction: string = "both"  # in, out or both
] {
  let mac = (mac-ip $mac)
  let dev = (pick-iface $iface $mac)
  let spec = (netem-spec $loss $delay $jitter)
  if ($spec | is-empty) {
    run-net ((clear-cmds $dev) | append (mark "netem off")) $"no impairment on ($dev)"
    return
  }
  let cmds = [
    (clear-cmds $dev)
    (add-cmds $dev $mac $spec $direction)
    (mark $"netem ($spec) \(($direction))")
  ] | flatten
  run-net $cmds $"netem ($spec) for the Mac on ($dev), direction ($direction)"
}

# Removes everything this script added.
def "main clear" [
  --mac: string
  --iface: string
] {
  let dev = (pick-iface $iface (mac-ip $mac))
  run-net ((clear-cmds $dev) | append (mark "netem off")) $"no impairment on ($dev)"
}

# Drops all traffic with the Mac for a while, then restores it. Prints the
# wall-clock time of the cut and the restore.
def "main cut" [
  --mac: string
  --iface: string
  --seconds: int = 10     # how long the Mac is unreachable
] {
  let mac = (mac-ip $mac)
  let dev = (pick-iface $iface $mac)
  let cmds = [
    (clear-cmds $dev)
    (add-cmds $dev $mac "loss 100%" "both")
    (mark $"cut ($seconds) s")
    'echo "cut      $(date +%H:%M:%S.%3N)"'
    $"sleep ($seconds)"
    (clear-cmds $dev)
    (mark "restored")
    'echo "restored $(date +%H:%M:%S.%3N)"'
  ] | flatten
  run-net $cmds $"traffic with the Mac on ($dev) cut for ($seconds) s"
}

# Shows the active qdiscs and filters. Needs no rights.
def "main show" [
  --mac: string
  --iface: string
] {
  let dev = if $iface != null { $iface } else { pick-iface null (mac-ip $mac) }
  print $"--- ($dev)"
  ^tc qdisc show dev $dev
  ^tc filter show dev $dev
  ^tc filter show dev $dev ingress
  if (^ip link show $IFB | complete | get exit_code) == 0 {
    print $"--- ($IFB) \(traffic from the Mac)"
    ^tc qdisc show dev $IFB
  }
}

# --- helpers ---------------------------------------------------------------

def home [] {
  if ($env.SUDO_USER? | is-not-empty) {
    ^getent passwd $env.SUDO_USER | split row ":" | get 5
  } else { $env.HOME }
}

def mac-ip [mac: any] {
  let ip = $mac | default ($env.BENCH_MAC? | default "")
  if ($ip | is-empty) {
    error make {msg: "which Mac? pass --mac <address> or set $env.BENCH_MAC"}
  }
  if not ($ip =~ '^\d{1,3}(\.\d{1,3}){3}$') {
    error make {msg: $"need the Mac's IPv4 address, not ($ip)"}
  }
  $ip
}

def pick-iface [iface: any, mac: string] {
  if $iface != null { return $iface }
  let dev = ^ip route get $mac | parse --regex 'dev (?<dev>\S+)' | get dev.0?
  if $dev == null { error make {msg: "can't find the interface: pass --iface"} }
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

def add-cmds [dev: string, mac: string, spec: string, direction: string] {
  if $direction not-in ["in" "out" "both"] {
    error make {msg: $"--direction must be in, out or both, not ($direction)"}
  }
  # Out: bands 1–3 keep the normal priomap; only packets to the Mac go to band 4.
  let out = if $direction in ["out" "both"] {
    [
      $"tc qdisc add dev ($dev) root handle 1: prio bands 4"
      $"tc qdisc add dev ($dev) parent 1:4 handle 40: netem ($spec)"
      $"tc filter add dev ($dev) parent 1: protocol ip prio 1 u32 match ip dst ($mac)/32 flowid 1:4"
    ]
  } else { [] }
  # In: only packets from the Mac take the detour through the ifb device.
  let inc = if $direction in ["in" "both"] {
    # With the capable tc we can't create the device: setup's root line does.
    if not (ifb-ready) and (caps-ready) and (^id -u | str trim) != "0" {
      error make {msg: $"($IFB) is missing, it's gone after a reboot: run `netem.nu setup`"}
    }
    let make = if (ifb-ready) { [] } else { [$"ip link add ($IFB) type ifb" $"ip link set ($IFB) up"] }
    $make | append [
      $"tc qdisc add dev ($IFB) root netem ($spec)"
      $"tc qdisc add dev ($dev) handle ffff: ingress"
      $"tc filter add dev ($dev) parent ffff: protocol ip prio 1 u32 match ip src ($mac)/32 action mirred egress redirect dev ($IFB)"
    ]
  } else { [] }
  $out | append $inc
}

# Removing what isn't there fails harmlessly; errors are hidden.
def clear-cmds [dev: string] {
  [
    $"tc qdisc del dev ($dev) root 2>/dev/null"
    $"tc qdisc del dev ($dev) ingress 2>/dev/null"
    $"tc qdisc del dev ($IFB) root 2>/dev/null"
  ]
}

# A shell command that adds a phase marker for scripts/bench/aether-log.nu.
# The file is created here, as the user, so root only appends to it.
def mark [text: string] {
  let dir = home | path join ".local/state/aetherdesk-bench"
  let file = $dir | path join "markers.log"
  if (^id -u | str trim) != "0" {
    mkdir $dir
    touch $file
  }
  $"echo \"$\(date +%Y-%m-%dT%H:%M:%S.%3N) ($text)\" >> ($file)"
}

def caps-ready [] {
  let f = home | path join $CAPS_DIR "tc"
  ($f | path exists) and ((^getcap $f | complete | get stdout) =~ 'cap_net_admin')
}

def ifb-ready [] {
  let r = ^ip -br link show $IFB | complete
  $r.exit_code == 0 and ($r.stdout =~ 'UP|UNKNOWN')
}

# Runs the commands with the capable tc/ip, or as root, or prints a sudo line.
def run-net [cmds: list<string>, what: string] {
  # `;` and not `&&`: the clear steps may fail when there's nothing to clear.
  let script = $cmds | str join "; "
  if (^id -u | str trim) == "0" {
    ^sh -c $script
    print $"done: ($what)"
  } else if (caps-ready) {
    let dir = home | path join $CAPS_DIR
    with-env {PATH: ($env.PATH | prepend $dir)} { ^sh -c $script }
    print $"done: ($what)"
  } else {
    print $"# ($what) — run this \(needs root, or run `netem.nu setup` once):"
    print $"sudo sh -c '($script)'"
  }
}
