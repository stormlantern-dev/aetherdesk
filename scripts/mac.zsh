#!/bin/zsh
# Everyday commands on the Mac (host) side. Run from anywhere in the repo.
#
#   scripts/mac.zsh update [branch]   fetch, switch to branch (default main), pull
#   scripts/mac.zsh doctor            permissions, display, IP addresses
#   scripts/mac.zsh serve [args]      build and run the host by hand (aether-agent serve)
#   scripts/mac.zsh record [args]     record a few seconds of H.264 to a file
#   scripts/mac.zsh pr <number>       switch to a PR's branch and run doctor
#   scripts/mac.zsh install           build AetherDesk.app, sign it and install it with the
#                                     daemon, agents and audio device (asks for sudo; same as
#                                     aether-agent install)
#   scripts/mac.zsh uninstall         remove them again (asks for sudo)
#   scripts/mac.zsh status            are the daemon and agent running? last log lines
#   scripts/mac.zsh restart           restart your agent (after granting permissions)
#   scripts/mac.zsh logs              follow the daemon and agent logs
#   scripts/mac.zsh pair              let a client pair: shows a code for 2 minutes (asks for sudo)
#   scripts/mac.zsh clients           list paired clients (asks for sudo)
#   scripts/mac.zsh unpair <name>     forget a client (asks for sudo)
set -euo pipefail
self=${0:A}
cd ${self:h:h}

APP=/Applications/AetherDesk.app
BIN=$APP/Contents/MacOS/aether-agent
LOGS=/Library/Logs/AetherDesk

agent() {
  cargo run --quiet --release -p aetherdesk-mac-host -- "$@"
}

# install, uninstall and status live in the binary (aether-agent install),
# so a Homebrew-installed host does the same; this builds it first.
built() {
  cargo build --quiet --release -p aetherdesk-mac-host
  if [[ $1 == install ]]; then
    # AetherDesk.app with the audio device; without Sparkle, since a
    # development build doesn't update itself.
    NO_SPARKLE=1 packaging/macos/app-bundle.sh target/release/aether-agent target/release/bundle > /dev/null
    target/release/bundle/AetherDesk.app/Contents/MacOS/aether-agent "$@"
  else
    target/release/aether-agent "$@"
  fi
}

case ${1:-help} in
  update)
    branch=${2:-main}
    git fetch --prune --quiet
    git switch --quiet "$branch"
    git pull --ff-only --quiet
    print "on $(git branch --show-current) at $(git log --oneline -1)"
    ;;
  doctor)
    agent doctor
    ;;
  serve | record)
    cmd=$1; shift
    agent "$cmd" "$@"
    ;;
  pr)
    [[ -n ${2:-} ]] || { print "usage: $self pr <number>"; exit 1; }
    branch=$(gh pr view "$2" --json headRefName --jq .headRefName)
    "$self" update "$branch"
    agent doctor
    ;;
  pair | clients | unpair)
    [[ -x $BIN ]] || { print "not installed; run: $self install (or use: aether-agent $1 --serve)"; exit 1; }
    cmd=$1; shift
    sudo "$BIN" "$cmd" "$@"
    ;;
  install | uninstall | status | restart) built "$1" ;;
  logs) tail -n 20 -F $LOGS/daemon.log $LOGS/agent.log $LOGS/loginwindow.log $LOGS/menu.log ;;
  *)
    sed -n '2,17p' "$self" | sed 's/^# \{0,1\}//'
    ;;
esac
