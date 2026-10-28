#!/usr/bin/env nu
# Drives the Mac's side of the benchmark (#33, docs/benchmark.md) from the
# Linux client over SSH, so one person (or Claude) at the Linux machine can
# run everything.
#
#   scripts/bench/mac.nu check                 can we reach it, is the repo there, clock offset
#   scripts/bench/mac.nu motion scroll         open the motion page in Safari (scroll, move, clock)
#   scripts/bench/mac.nu cpu aether            mac-cpu.nu on the Mac, 10 s
#   scripts/bench/mac.nu lock                  lock the screen (display sleep)
#   scripts/bench/mac.nu run 'git status'      any command, in the repo on the Mac
#
# Needs an SSH host alias (default aether-mac, or $env.BENCH_SSH) in
# ~/.ssh/config that logs in with a key; it never asks for a password. The
# repo on the Mac is $env.BENCH_MAC_REPO (default ~/Documents/projects/aetherdesk).
# Addresses and user names stay in ~/.ssh/config, out of the repo.

def main [] {
  print (help main)
}

# Checks the connection, the repo on the Mac and the clock difference.
def "main check" [] {
  print $"host alias: (ssh-host)"
  print (remote "echo \"reachable: macOS $(sw_vers -productVersion), $(uname -m)\"")
  print (remote "test -d . && git log --oneline -1 && echo \"nu: $(command -v nu || echo MISSING)\"")
  let off = (clock-offset)
  print $"clock: Mac minus Linux ≈ ($off.offset_ms) ms \(± ($off.rtt_ms / 2) ms, round trip ($off.rtt_ms) ms)"
}

# Opens scripts/bench/motion.html on the Mac in Safari.
def "main motion" [
  mode: string = "scroll"   # scroll, move or clock
  --speed: int = 600        # scroll speed in px/s
] {
  if $mode not-in ["scroll" "move" "clock"] {
    error make {msg: "mode is scroll, move or clock"}
  }
  remote $"open -a Safari \"file://$PWD/scripts/bench/motion.html?speed=($speed)#($mode)\"" | print
  print $"motion page open on the Mac: ($mode). Put Safari full screen there \(Ctrl+Cmd+F) once."
}

# Runs scripts/bench/mac-cpu.nu on the Mac.
def "main cpu" [
  tool: string               # aether, screensharing, rustdesk, sunshine, parsec, or a regex
  --seconds (-s): int = 10
] {
  remote $"scripts/bench/mac-cpu.nu ($tool) --seconds ($seconds)" | print
}

# Locks the Mac: puts the display to sleep (locks when "require password
# immediately" is on, the default with FileVault).
def "main lock" [] {
  remote "pmset displaysleepnow" | print
  print "display asleep; the Mac locks if it asks for the password immediately"
}

# Any command, run by zsh in the repo on the Mac. No single quotes in it.
def "main run" [cmd: string] {
  remote $cmd | print
}

# --- helpers ---------------------------------------------------------------

def ssh-host [] { $env.BENCH_SSH? | default "aether-mac" }

# Key only, never a password prompt; one shared connection, so repeated
# calls (and the clock check) don't pay for a new handshake each time.
def ssh-opts [] {
  [
    "-o" "BatchMode=yes" "-o" "ConnectTimeout=5"
    "-o" "ControlMaster=auto" "-o" "ControlPath=~/.ssh/cm-%C" "-o" "ControlPersist=120"
  ]
}

def repo [] { $env.BENCH_MAC_REPO? | default "~/Documents/projects/aetherdesk" }

# The Mac's login shell may be nushell: wrap everything in `zsh -lc '…'`,
# which reads the same in nu and in sh, and gives Homebrew's PATH.
def remote [cmd: string] {
  if ($cmd | str contains "'") {
    error make {msg: "no single quotes in remote commands"}
  }
  let r = ^ssh ...(ssh-opts) (ssh-host) $"zsh -lc 'cd (repo) && ($cmd)'" | complete
  if $r.exit_code != 0 {
    error make {msg: $"on the Mac \(exit ($r.exit_code)): ($r.stderr | str trim)"}
  }
  $r.stdout | str trim --right
}

# Mac clock minus Linux clock, from the fastest of a few round trips.
def clock-offset [] {
  remote "true" | ignore  # opens the shared connection
  1..5 | each {|_|
    let t0 = (date now | into int) / 1_000_000
    let mac = (^ssh ...(ssh-opts) (ssh-host) "perl -MTime::HiRes=time -e 'printf q(%.3f), time'" | into float) * 1000
    let t1 = (date now | into int) / 1_000_000
    {rtt_ms: (($t1 - $t0) | math round), offset_ms: (($mac - ($t0 + $t1) / 2) | math round)}
  } | sort-by rtt_ms | first
}
