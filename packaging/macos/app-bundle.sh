#!/usr/bin/env bash
# Builds AetherDesk.app around an aether-agent binary (#28, #115):
#
#   AetherDesk.app/Contents/
#     Info.plist                     from `aether-agent _info-plist`
#     MacOS/aether-agent             daemon, agents, menu and CLI in one
#     Resources/AetherDesk.icns, MenuBar*Template*.png
#     PlugIns/AetherDesk.driver      the audio device (build-driver.sh)
#     Frameworks/Sparkle.framework   the updater (fetch-sparkle.sh)
#
# and signs it ad hoc, inside out; sign-notarize.sh signs it again with the
# Developer ID. Run on a Mac from the repository root.
#
#   packaging/macos/app-bundle.sh <aether-agent> <outdir>
#
# SPARKLE_ED_PUBLIC_KEY   the public EdDSA key from Sparkle's generate_keys.
#                         Without it the app has no updater settings and
#                         updates are off (a development build).
# RELEASE=1               fail unless SPARKLE_ED_PUBLIC_KEY is a real key.
# NO_SPARKLE=1            leave Sparkle.framework out (quicker local builds).
# SPARKLE_CACHE           where fetch-sparkle.sh keeps the download
#                         (default: target/sparkle).
set -euo pipefail

bin=${1:?usage: app-bundle.sh <aether-agent> <outdir>}
outdir=${2:?usage: app-bundle.sh <aether-agent> <outdir>}
here=$(cd "$(dirname "$0")" && pwd)
app=$outdir/AetherDesk.app
contents=$app/Contents

key=${SPARKLE_ED_PUBLIC_KEY:-}
if [ "${RELEASE:-}" = 1 ]; then
	if [ -z "$key" ]; then
		echo "error: RELEASE=1 needs SPARKLE_ED_PUBLIC_KEY, the public key from Sparkle's" >&2
		echo "generate_keys (the release environment's variable): see docs/RELEASING.md" >&2
		exit 1
	fi
	[ -z "${NO_SPARKLE:-}" ] || { echo "error: a release needs Sparkle (unset NO_SPARKLE)" >&2; exit 1; }
fi

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$bin" "$contents/MacOS/aether-agent"
if [ -n "$key" ]; then
	"$contents/MacOS/aether-agent" _info-plist --ed-key "$key" > "$contents/Info.plist"
else
	"$contents/MacOS/aether-agent" _info-plist > "$contents/Info.plist"
fi
plutil -lint "$contents/Info.plist" > /dev/null
printf 'APPL????' > "$contents/PkgInfo"

cp "$here/icon/AetherDesk.icns" "$contents/Resources/"
cp "$here"/icon/menubar/*.png "$contents/Resources/"
mkdir -p "$contents/PlugIns"
"$here/build-driver.sh" "$contents/PlugIns" > /dev/null

if [ -z "${NO_SPARKLE:-}" ]; then
	sparkle=$("$here/fetch-sparkle.sh" "${SPARKLE_CACHE:-${CARGO_TARGET_DIR:-target}/sparkle}")
	mkdir -p "$contents/Frameworks"
	ditto "$sparkle/Sparkle.framework" "$contents/Frameworks/Sparkle.framework"
	cp "$sparkle/LICENSE" "$contents/Resources/Sparkle-LICENSE.txt"
	fw=$contents/Frameworks/Sparkle.framework
	# The XPC services are for sandboxed apps (SUEnableInstallerLauncherService
	# and SUEnableDownloaderService, both off here); the headers are for
	# building against it. Less code to sign and ship.
	rm -rf "$fw/Versions/B/XPCServices" "$fw/XPCServices" \
		"$fw/Versions/B/Headers" "$fw/Headers" \
		"$fw/Versions/B/PrivateHeaders" "$fw/PrivateHeaders" \
		"$fw/Versions/B/Modules" "$fw/Modules"
fi

# Ad hoc, inside out (no --deep: it signs nested code the wrong way round).
"$here/sign-app.sh" "$app" -
codesign --verify --deep --strict "$app"
echo "$app"
