#!/usr/bin/env bash
# Builds the Mac host (auroval-agent, Apple Silicon) and the Auroval audio
# device (Auroval.driver), signs them and packs them with the licences
# and INSTALL.md into
# <outdir>/auroval-host-<version>-arm64-macos.tar.gz.
# Run from the repository root on a Mac (the release workflow does).
#
#   packaging/macos/release-tarball.sh <version> <outdir>
#
# SIGNING=developer-id signs and notarizes with sign-notarize.sh (see there
# for the variables it needs); the release workflow always does. Otherwise
# it signs ad hoc, for trying the packaging locally.
set -euo pipefail

version=${1:?usage: release-tarball.sh <version> <outdir>}
outdir=${2:?usage: release-tarball.sh <version> <outdir>}
name=auroval-host-$version-arm64-macos
target=${CARGO_TARGET_DIR:-target}

cargo build --release --locked -p auroval-mac-host --target aarch64-apple-darwin

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -p "$dir"
cp "$target/aarch64-apple-darwin/release/auroval-agent" "$dir/"
# Next to the binary: `auroval-agent install` installs it from there.
packaging/macos/build-driver.sh "$dir" > /dev/null
if [ "${SIGNING:-}" = developer-id ]; then
	packaging/macos/sign-notarize.sh "$dir/auroval-agent" "$dir/Auroval.driver"
else
	# Ad hoc: `auroval-agent install` signs its own copy again, with the
	# user's identity when they have one.
	codesign --force --sign - --identifier nl.stormlantern.auroval.agent "$dir/auroval-agent"
fi
cp LICENSE-APACHE LICENSE-MIT packaging/macos/INSTALL.md "$dir/"

mkdir -p "$outdir"
tar -C "$stage" -czf "$outdir/$name.tar.gz" "$name"
(cd "$outdir" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$outdir/$name.tar.gz"
