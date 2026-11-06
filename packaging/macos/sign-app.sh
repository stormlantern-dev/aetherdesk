#!/usr/bin/env bash
# Signs AetherDesk.app inside out: the audio device, Sparkle's helpers and
# framework, then the app (whose signature seals the rest). With a real
# identity: hardened runtime and a secure timestamp, as notarization wants.
# The app's identifier is its bundle identifier (Info.plist).
#
#   packaging/macos/sign-app.sh <AetherDesk.app> <identity, or - for ad hoc>
set -euo pipefail

app=${1:?usage: sign-app.sh <AetherDesk.app> <identity>}
identity=${2:?usage: sign-app.sh <AetherDesk.app> <identity>}
contents=$app/Contents
fw=$contents/Frameworks/Sparkle.framework

opts=(--force --sign "$identity")
[ "$identity" = - ] || opts+=(--options runtime --timestamp)

sign() { codesign "${opts[@]}" "$@"; }

[ -d "$contents/PlugIns/AetherDesk.driver" ] && sign "$contents/PlugIns/AetherDesk.driver"
if [ -d "$fw" ]; then
	sign "$fw/Versions/B/Autoupdate"
	sign "$fw/Versions/B/Updater.app"
	sign "$fw"
fi
sign "$app"
