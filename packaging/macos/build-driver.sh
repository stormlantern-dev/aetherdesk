#!/usr/bin/env bash
# Builds the AetherDesk audio device (crates/mac-host/driver) into
# <outdir>/AetherDesk.driver. app-bundle.sh puts it in the app's
# Contents/PlugIns, where `aether-agent install` and the installer take it
# from. Signs it ad hoc; sign-app.sh signs it again with the Developer ID.
#
#   packaging/macos/build-driver.sh <outdir>
set -euo pipefail

outdir=${1:?usage: build-driver.sh <outdir>}
src=$(cd "$(dirname "$0")/../../crates/mac-host/driver" && pwd)
bundle=$outdir/AetherDesk.driver

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS"
clang -arch arm64 -O2 -Wall -Wextra -Wno-unused-parameter -Werror -mmacosx-version-min=14.0 \
	-bundle -o "$bundle/Contents/MacOS/AetherDesk" "$src/driver.c" \
	-framework CoreFoundation -framework CoreAudio
cp "$src/Info.plist" "$bundle/Contents/"
codesign --force --sign - "$bundle"
echo "$bundle"
