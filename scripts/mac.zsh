#!/bin/zsh
# Everyday commands on the Mac (host) side. Run from anywhere in the repo.
#
#   scripts/mac.zsh update [branch]   fetch, switch to branch (default main), pull
#   scripts/mac.zsh doctor            permissions, display, IP addresses
#   scripts/mac.zsh serve [args]      build and run the host (aether-agent serve)
#   scripts/mac.zsh record [args]     record a few seconds of H.264 to a file
#   scripts/mac.zsh pr <number>       switch to a PR's branch and run doctor
set -euo pipefail
self=${0:A}
cd ${self:h:h}

agent() {
  cargo run --quiet --release -p aetherdesk-mac-host -- "$@"
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
  *)
    sed -n '2,8p' "$self" | sed 's/^# \{0,1\}//'
    ;;
esac
