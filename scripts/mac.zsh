#!/bin/zsh
# Everyday commands on the Mac (host) side. Run from anywhere in the repo.
#
#   scripts/mac.zsh update [branch]   fetch, switch to branch (default main), pull
#   scripts/mac.zsh doctor            permissions, display, IP addresses
#   scripts/mac.zsh serve [args]      build and run the host by hand (aether-agent serve)
#   scripts/mac.zsh record [args]     record a few seconds of H.264 to a file
#   scripts/mac.zsh pr <number>       switch to a PR's branch and run doctor
#   scripts/mac.zsh install           build, sign and install the daemon + agents (asks for sudo)
#   scripts/mac.zsh uninstall         remove them again (asks for sudo)
#   scripts/mac.zsh status            are the daemon and agent running? last log lines
#   scripts/mac.zsh logs              follow the daemon and agent logs
#   scripts/mac.zsh pair              let a client pair: shows a code for 2 minutes (asks for sudo)
#   scripts/mac.zsh clients           list paired clients (asks for sudo)
#   scripts/mac.zsh unpair <name>     forget a client (asks for sudo)
set -euo pipefail
self=${0:A}
cd ${self:h:h}

LABEL=nl.stormlantern.aetherdesk
DEST="/Library/Application Support/AetherDesk"
BIN="$DEST/aether-agent"
LOGS=/Library/Logs/AetherDesk
DAEMON_PLIST=/Library/LaunchDaemons/$LABEL.daemon.plist
AGENT_PLIST=/Library/LaunchAgents/$LABEL.agent.plist
LOGINWINDOW_PLIST=/Library/LaunchAgents/$LABEL.loginwindow.plist
# Self-signed code-signing certificate, so permissions survive rebuilds.
IDENTITY=${AETHERDESK_IDENTITY:-"AetherDesk Dev"}

# unload JOB: bootout, then wait until launchd has really let go. bootout
# returns before the job is gone, and a bootstrap right after it fails
# with "Bootstrap failed: 5: Input/output error".
unload() {
  local job=$1 i
  launchctl bootout $job 2>/dev/null || return 0
  for i in {1..50}; do
    launchctl print $job >/dev/null 2>&1 || return 0
    sleep 0.2
  done
  print "warning: $job is still loaded after 10 s"
}

agent() {
  cargo run --quiet --release -p aetherdesk-mac-host -- "$@"
}

# plist LABEL SESSION_TYPE(or "") LOG ARGS...
plist() {
  local label=$1 stype=$2 log=$3; shift 3
  print '<?xml version="1.0" encoding="UTF-8"?>'
  print '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
  print '<plist version="1.0"><dict>'
  print "  <key>Label</key><string>$label</string>"
  print '  <key>ProgramArguments</key><array>'
  print "    <string>$BIN</string>"
  for a in "$@"; do print "    <string>$a</string>"; done
  print '  </array>'
  if [[ -n $stype ]]; then print "  <key>LimitLoadToSessionType</key><string>$stype</string>"; fi
  print '  <key>RunAtLoad</key><true/>'
  print '  <key>KeepAlive</key><true/>'
  print '  <key>ProcessType</key><string>Interactive</string>'
  print "  <key>StandardOutPath</key><string>$LOGS/$log</string>"
  print "  <key>StandardErrorPath</key><string>$LOGS/$log</string>"
  print '</dict></plist>'
}

install() {
  # No -v: that lists only trusted identities, and a self-signed certificate
  # usually isn't, though codesign signs with it fine.
  if ! security find-identity -p codesigning | grep -qF "\"$IDENTITY\""; then
    print "No code-signing identity \"$IDENTITY\". Create one in Keychain Access →"
    print "Certificate Assistant → Create a Certificate (type: Code Signing), or set"
    print "AETHERDESK_IDENTITY to the name of one you have. See docs/MVP-PLAN.md."
    exit 1
  fi
  cargo build --quiet --release -p aetherdesk-mac-host
  local stage=$(mktemp -d)
  cp target/release/aether-agent $stage/
  # One identifier for every build, so permissions stick to it.
  codesign --force --sign "$IDENTITY" --identifier $LABEL.agent $stage/aether-agent
  plist $LABEL.daemon "" daemon.log daemon >$stage/daemon.plist
  plist $LABEL.agent Aqua agent.log agent >$stage/agent.plist
  plist $LABEL.loginwindow LoginWindow loginwindow.log agent >$stage/loginwindow.plist
  print "Installing needs admin rights; macOS asks for your password."
  sudo "$self" _install-root $stage
  rm -rf $stage
  # The user's agent: launchd loads it by itself at every login; this
  # starts it in the current session.
  unload gui/$UID/$LABEL.agent
  launchctl bootstrap gui/$UID $AGENT_PLIST
  print "\nInstalled. Once, grant $BIN"
  print "both Screen Recording and Accessibility in System Settings → Privacy & Security"
  print "(+ button, then Cmd+Shift+G and paste the path), then run: $self status"
  print "To let a client in: $self pair"
}

# Runs as root, from install.
install_root() {
  local stage=$1
  mkdir -p $DEST $LOGS
  chmod 1777 $LOGS # the user's agent writes here too
  unload system/$LABEL.daemon
  # New inode on every install: overwriting a signed binary in place can get it killed.
  rm -f $BIN
  cp $stage/aether-agent $BIN
  chown root:wheel $BIN
  chmod 755 $BIN
  # The user's agent runs the binary from here, so the directory stays
  # readable; the host key and the pairing files in it are root-only (0600).
  rm -f $DEST/token # M2's shared token, replaced by pairing
  cp $stage/daemon.plist $DAEMON_PLIST
  cp $stage/agent.plist $AGENT_PLIST
  cp $stage/loginwindow.plist $LOGINWINDOW_PLIST
  chown root:wheel $DAEMON_PLIST $AGENT_PLIST $LOGINWINDOW_PLIST
  chmod 644 $DAEMON_PLIST $AGENT_PLIST $LOGINWINDOW_PLIST
  launchctl bootstrap system $DAEMON_PLIST
}

uninstall() {
  unload gui/$UID/$LABEL.agent
  print "Uninstalling needs admin rights; macOS asks for your password."
  sudo "$self" _uninstall-root
  print "Removed. The aether-agent entries in Privacy & Security stay until you remove them."
}

# Runs as root, from uninstall.
uninstall_root() {
  unload system/$LABEL.daemon
  rm -f $DAEMON_PLIST $AGENT_PLIST $LOGINWINDOW_PLIST
  rm -rf $DEST $LOGS /var/run/aetherdesk
}

status() {
  local job
  for job in system/$LABEL.daemon gui/$UID/$LABEL.agent; do
    if launchctl print $job >/dev/null 2>&1; then
      print "$job: $(launchctl print $job | grep -E '^\s*(state|pid) =' | tr -s ' \n' ' ')"
    else
      print "$job: not loaded"
    fi
  done
  local log
  for log in daemon agent loginwindow; do
    [[ -f $LOGS/$log.log ]] || continue
    print "\n--- $LOGS/$log.log"
    tail -n 8 $LOGS/$log.log
  done
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
  install) install ;;
  uninstall) uninstall ;;
  status) status ;;
  logs) tail -n 20 -F $LOGS/daemon.log $LOGS/agent.log $LOGS/loginwindow.log ;;
  _install-root) install_root $2 ;;
  _uninstall-root) uninstall_root ;;
  *)
    sed -n '2,15p' "$self" | sed 's/^# \{0,1\}//'
    ;;
esac
