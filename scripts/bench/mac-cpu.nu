#!/usr/bin/env nu
# CPU use of a remote-desktop tool on the Mac, for the scroll test in #33
# (docs/benchmark.md). Run it on the Mac, start scrolling on the client.
#
#   scripts/bench/mac-cpu.nu aether              AetherDesk (daemon + agent)
#   scripts/bench/mac-cpu.nu screensharing       Apple Screen Sharing (Pippin, VNC)
#   scripts/bench/mac-cpu.nu rustdesk --seconds 20
#   scripts/bench/mac-cpu.nu 'myproc|other'      any regex on the process name
#
# Samples `top` once a second (no root needed) and prints the average and
# peak of the tool's processes together, per process, and WindowServer
# apart (screen capture makes it busier for every tool). 100 % = one core.

const TOOLS = {
  aether: 'aether-agent'
  screensharing: '(?i)screensharing|ssinvitation'
  rustdesk: '(?i)rustdesk'
  sunshine: '(?i)sunshine'
  parsec: '(?i)parsec'
}

def main [
  tool: string             # aether, screensharing, rustdesk, sunshine, parsec, or a regex
  --seconds (-s): int = 10 # how long to sample
] {
  let pattern = $TOOLS | get -o $tool | default $tool
  print $"sampling ($seconds) s for /($pattern)/ — scroll now…"
  # The first sample has no CPU figures yet (top needs two to compare): skip it.
  let samples = ^top -l ($seconds + 1) -s 1 -stats cpu,command
    | split row --regex '(?m)^Processes:'
    | skip 2
    | each {|block| procs $block }
  if ($samples | is-empty) { error make {msg: "top gave no samples"} }

  let sum = {|s, re| $s | where {|p| $p.command =~ $re } | get cpu | math sum }
  let tool_cpu = $samples | each {|s| do $sum $s $pattern }
  let ws_cpu = $samples | each {|s| do $sum $s '^WindowServer$' }
  let per_process = $samples | flatten | where {|p| $p.command =~ $pattern }
    | group-by command --to-table
    | each {|g| {process: $g.command, "avg %CPU": ($g.items.cpu | math sum | $in / ($samples | length) | math round --precision 1)} }

  print ""
  print ($per_process | table)
  let r = {|v| $v | math round --precision 1 }
  print $"($tool): avg (do $r ($tool_cpu | math avg)) %, peak (do $r ($tool_cpu | math max)) % · WindowServer avg (do $r ($ws_cpu | math avg)) % · ($samples | length) samples"
}

# Rows of one top sample: {cpu, command}.
def procs [block: string] {
  let lines = $block | lines
  let header = $lines | enumerate | where {|l| $l.item =~ '^\s*%CPU' } | get -o 0.index
  if $header == null { return [] }
  $lines | skip ($header + 1) | each {|l|
    $l | parse --regex '^\s*(?<cpu>[\d.]+)\s+(?<command>.+?)\s*$' | get -o 0
  } | compact | each {|p| {cpu: ($p.cpu | into float), command: $p.command} }
}
