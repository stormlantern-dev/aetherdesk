#!/bin/zsh
# Installs the spike: the plug-in for coreaudiod and the reader as a launchd
# daemon. Run with sudo after build.zsh. Restarting coreaudiod cuts all
# sound on the Mac for a moment.
set -euo pipefail
cd ${0:A:h}
[[ $EUID == 0 ]] || { echo "run with sudo"; exit 1 }
label=nl.stormlantern.auroval.audio-spike
dest="/Library/Application Support/Auroval/audio-spike"
mkdir -p $dest
install -m 755 build/reader $dest/reader
install -m 644 $label.plist /Library/LaunchDaemons/$label.plist
launchctl bootout system/$label 2>/dev/null || true
: > /tmp/auroval-audio-spike.log
chmod 644 /tmp/auroval-audio-spike.log
launchctl bootstrap system /Library/LaunchDaemons/$label.plist
rm -rf /Library/Audio/Plug-Ins/HAL/Auroval.driver
cp -R build/Auroval.driver /Library/Audio/Plug-Ins/HAL/
chown -R root:wheel /Library/Audio/Plug-Ins/HAL/Auroval.driver
killall coreaudiod
echo "installed; the Auroval device should appear in a few seconds"
