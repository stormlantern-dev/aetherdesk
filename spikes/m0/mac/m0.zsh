#!/bin/zsh
# M0 spike helper — run this on the Mac. See ../README.md.
set -euo pipefail

HERE=${0:A:h}
CRATE=${HERE:h}
DEST=/Users/Shared/aetherdesk
BIN=$DEST/m0
OUT=/Users/Shared/aetherdesk-m0
LABEL=nl.stormlantern.aetherdesk.m0
IDENTITY=${M0_IDENTITY:-"AetherDesk Dev"}

# plist LABEL SESSION_TYPE(or "") RUN_AT_LOAD(true|false) ARGS...
plist() {
  local label=$1 stype=$2 ral=$3; shift 3
  print '<?xml version="1.0" encoding="UTF-8"?>'
  print '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
  print '<plist version="1.0"><dict>'
  print "  <key>Label</key><string>$label</string>"
  print '  <key>ProgramArguments</key><array>'
  print "    <string>$BIN</string>"
  for a in "$@"; do print "    <string>$a</string>"; done
  print '  </array>'
  if [[ -n $stype ]]; then print "  <key>LimitLoadToSessionType</key><string>$stype</string>"; fi
  print "  <key>RunAtLoad</key><$ral/>"
  print "  <key>StandardOutPath</key><string>$DEST/$label.out</string>"
  print "  <key>StandardErrorPath</key><string>$DEST/$label.out</string>"
  print '</dict></plist>'
}

install() {
  (cd $CRATE && cargo build --release)
  mkdir -p $DEST $OUT
  chmod 777 $OUT
  # New inode on every install: overwriting a signed binary in place can get it killed.
  rm -f $BIN
  cp $CRATE/target/release/m0 $BIN
  codesign --force --sign "$IDENTITY" --identifier $LABEL $BIN
  codesign -dv --verbose=2 $BIN 2>&1 | grep -E '^(Identifier|Authority|Signature)'

  local lock=(probe --seconds 90 --type-after 30)
  mkdir -p ~/Library/LaunchAgents
  plist $LABEL.hid Aqua false $lock --tap hid >~/Library/LaunchAgents/$LABEL.hid.plist
  plist $LABEL.session Aqua false $lock --tap session >~/Library/LaunchAgents/$LABEL.session.plist
  plist $LABEL.root "" false $lock --tap hid >$DEST/$LABEL.root.plist
  plist $LABEL.loginwindow LoginWindow true probe --seconds 120 --type-after 45 --tap hid \
    >$DEST/$LABEL.loginwindow.plist
  print "\nInstalled $BIN. Grant it Screen Recording and Accessibility (see README) if not done yet."
}

start_user() {
  local job=$LABEL.$1
  launchctl bootout gui/$UID/$job 2>/dev/null || true
  launchctl bootstrap gui/$UID ~/Library/LaunchAgents/$job.plist
  launchctl kickstart gui/$UID/$job
  lock_hint
}

start_root() {
  local job=$LABEL.root
  sudo cp $DEST/$job.plist /Library/LaunchDaemons/
  sudo chown root:wheel /Library/LaunchDaemons/$job.plist
  sudo launchctl bootout system/$job 2>/dev/null || true
  sudo launchctl bootstrap system /Library/LaunchDaemons/$job.plist
  sudo launchctl kickstart system/$job
  lock_hint
}

lock_hint() {
  print "Running for 90 s. Lock the screen NOW (Ctrl+Cmd+Q)."
  print "At ~30 s it types 'aetherdesk' (no Return) — watch the password field for dots."
  print "Afterwards: clear the field, unlock, and run: $0 latest"
}

loginwindow_on() {
  local job=$LABEL.loginwindow
  sudo cp $DEST/$job.plist /Library/LaunchAgents/
  sudo chown root:wheel /Library/LaunchAgents/$job.plist
  print "Now log out (Apple menu → Log Out). The probe starts with the login window,"
  print "captures for 120 s and types at ~45 s: click your user so the password field is active."
  print "Afterwards: clear the field, log in, run: $0 latest  — and then: $0 loginwindow-off"
}

latest() {
  local dirs=($OUT/*(N/om))
  (( $#dirs )) || { print "no runs in $OUT"; return 1; }
  print "${#dirs} run(s); newest first:"
  for d in ${dirs[1,4]}; do print "  $d  ($(ls $d/*.png 2>/dev/null | wc -l | tr -d ' ') frames)"; done
  print "\n--- ${dirs[1]}/log.txt"
  grep -vE 'frame [0-9]+ ok' ${dirs[1]}/log.txt | head -40
  print "($(grep -cE 'frame [0-9]+ ok' ${dirs[1]}/log.txt) ok frames hidden)"
  open ${dirs[1]}
}

cleanup() {
  for j in hid session; do
    launchctl bootout gui/$UID/$LABEL.$j 2>/dev/null || true
    rm -f ~/Library/LaunchAgents/$LABEL.$j.plist
  done
  sudo launchctl bootout system/$LABEL.root 2>/dev/null || true
  sudo rm -f /Library/LaunchDaemons/$LABEL.root.plist /Library/LaunchAgents/$LABEL.loginwindow.plist
  print "Removed launchd jobs. Binary and results stay in $DEST and $OUT."
}

case ${1:-} in
  install) install ;;
  lock-hid) start_user hid ;;
  lock-session) start_user session ;;
  lock-root) start_root ;;
  loginwindow-on) loginwindow_on ;;
  loginwindow-off) sudo rm -f /Library/LaunchAgents/$LABEL.loginwindow.plist ;;
  latest) latest ;;
  cleanup) cleanup ;;
  *) print "usage: $0 install|lock-hid|lock-session|lock-root|loginwindow-on|loginwindow-off|latest|cleanup"; exit 1 ;;
esac
