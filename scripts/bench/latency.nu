#!/usr/bin/env nu
# Glass-to-glass latency from stopwatch photos (the method from #7): a
# millisecond stopwatch runs on the Mac, its picture in the client is next to
# it, one photo shows both. Per photo, read both numbers off; this prints the
# table and the median, ready to paste.
#
#   scripts/bench/latency.nu 3.010 2.945 5.121 5.056 7.138 7.106
#   "3.010 2.945\n5.121 5.056 ghosted" | scripts/bench/latency.nu --tool Pippin
#   open photos.txt | scripts/bench/latency.nu
#
# Each pair is: what the Mac shows, what the client shows. Times are seconds
# (3.010) or minutes:seconds (1:03.010). On stdin, one photo per line; any
# words after the two numbers end up as a note in the table. Lines starting
# with # are skipped.

def main [
  ...times: string     # mac client mac client …, if not piped in
  --tool (-t): string  # name for the heading
] {
  # Read stdin only without arguments, and only when piped: on a terminal it would wait.
  let rows = if not ($times | is-empty) {
    from-args $times
  } else if (is-terminal --stdin) {
    []
  } else {
    from-lines (^cat)
  }
  if ($rows | is-empty) {
    error make {msg: "no photos: give mac/client pairs as arguments or one pair per line on stdin"}
  }

  let rows = $rows | enumerate | each {|r|
    let diff = (($r.item.mac - $r.item.client) * 1000 | math round)
    {photo: ($r.index + 1), mac: $r.item.mac_text, client: $r.item.client_text, ms: $diff, note: $r.item.note}
  }
  let diffs = $rows | get ms
  let median = $diffs | math median
  let heading = if $tool == null { "Latency" } else { $"Latency — ($tool)" }

  let table = $rows | each {|r|
    let note = if ($r.note | is-empty) { "" } else { $" \(($r.note))" }
    $"| ($r.photo) | ($r.mac) | ($r.client) | ($r.ms) ms($note) |"
  }
  let warn = if ($diffs | any {|d| $d < 0 }) {
    ["" "Warning: a negative difference means the client shows a later time than the Mac; check that the columns aren't swapped."]
  } else { [] }
  let few = if ($rows | length) < 5 { ["" $"Note: only ($rows | length) photos; #33 asks for at least 5."] } else { [] }

  [
    $"**($heading)**"
    ""
    "| photo | Mac | client | difference |"
    "|---|---|---|---|"
    ...$table
    ""
    $"Median **($median) ms** over ($rows | length) photos \(min ($diffs | math min), max ($diffs | math max))."
    ...$warn
    ...$few
  ] | str join "\n"
}

def from-args [times: list<string>] {
  if ($times | length) mod 2 != 0 {
    error make {msg: "give the times in pairs: mac client mac client …"}
  }
  $times | chunks 2 | each {|p| pair $p.0 $p.1 "" }
}

def from-lines [text: string] {
  $text | lines | str trim | where {|l| $l != "" and not ($l | str starts-with "#") } | each {|l|
    let words = $l | split row --regex '\s+'
    if ($words | length) < 2 {
      error make {msg: $"need two times on a line, got: ($l)"}
    }
    pair $words.0 $words.1 ($words | skip 2 | str join " ")
  }
}

def pair [mac: string, client: string, note: string] {
  {mac: (seconds $mac), client: (seconds $client), mac_text: $mac, client_text: $client, note: $note}
}

# "3.010" → 3.01, "1:03.010" → 63.01
def seconds [t: string] {
  let parts = $t | split row ":" | each {|p| $p | into float }
  $parts | reduce --fold 0.0 {|p, acc| $acc * 60 + $p }
}
