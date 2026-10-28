#!/usr/bin/env nu
# Posts host output as a comment on an issue or PR, so nothing has to be
# retyped. Works for both: a PR is an issue to the GitHub API.
#
#   scripts/report.nu 19 logs              last lines of the daemon and agent logs
#   scripts/report.nu 3 m0                 the newest M0 spike run
#   some-command | scripts/report.nu 19    whatever is piped in
#   scripts/report.nu 19 logs --dry-run    show the comment, don't post it
#
# $AETHERDESK_TOKEN is replaced by <token> before posting: the repo is public.

const LOGS = "/Library/Logs/AetherDesk"

def main [
  number: int             # issue or PR number
  what?: string           # logs, m0, or nothing to read from stdin
  --title (-t): string    # first line of the comment (default: what)
  --lines (-n): int = 40  # lines per log file
  --dry-run               # print the comment instead of posting it
] {
  let root = $env.FILE_PWD | path dirname
  cd $root

  let text = match $what {
    "logs" => { logs $lines }
    "m0" => { capture ($root | path join spikes/m0/mac/m0.zsh) latest }
    null => {
      # Read stdin only here, and only when piped: on a terminal it would wait.
      let piped = if (is-terminal --stdin) { "" } else { ^cat }
      if ($piped | str trim | is-empty) {
        error make {msg: "nothing to post: pipe something in, or say logs or m0"}
      }
      $piped
    }
    _ => { error make {msg: $"don't know ($what): use logs, m0, or pipe something in"} }
  }

  let token = $env.AETHERDESK_TOKEN? | default ""
  let text = if ($token | is-empty) { $text } else { $text | str replace --all $token "<token>" }

  let mac = try { ^sw_vers -productVersion | str trim } catch { "?" }
  let commit = ^git log --oneline -1 | str trim
  let fence = "```"
  let body = [
    $"**($title | default ($what | default 'output'))** — macOS ($mac), at `($commit)`"
    $fence
    ($text | str trim --right)
    $fence
  ] | str join "\n"

  if $dry_run {
    print $body
    return
  }
  let url = {body: $body}
    | to json
    | ^gh api $"repos/{owner}/{repo}/issues/($number)/comments" --input - --jq .html_url
  print $"posted: ($url | str trim)"
}

# Last lines of each host log that exists.
def logs [lines: int] {
  let parts = ["daemon" "agent" "loginwindow"]
    | each {|name| $"($LOGS)/($name).log" }
    | where {|file| $file | path exists }
    | each {|file| $"--- ($file)\n(^tail -n $lines $file)" }
  if ($parts | is-empty) { $"no logs in ($LOGS)" } else { $parts | str join "\n\n" }
}

# Runs a command and keeps stdout and stderr, even when it fails.
def capture [cmd: string, ...args: string] {
  let r = ^$cmd ...$args | complete
  $"($r.stdout)($r.stderr)"
}
