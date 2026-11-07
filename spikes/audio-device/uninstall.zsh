#!/bin/zsh
# Removes everything install.zsh put in place. Run with sudo.
set -euo pipefail
[[ $EUID == 0 ]] || { echo "run with sudo"; exit 1 }
label=nl.stormlantern.auroval.audio-spike
launchctl bootout system/$label 2>/dev/null || true
rm -f /Library/LaunchDaemons/$label.plist
rm -rf "/Library/Application Support/Auroval/audio-spike"
rm -rf /Library/Audio/Plug-Ins/HAL/Auroval.driver
killall coreaudiod
echo "removed"
