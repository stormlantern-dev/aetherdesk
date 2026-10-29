#!/usr/bin/env bash
# Builds the Mac host (aether-agent, Apple Silicon), signs it ad hoc with the
# identifier `aether-agent install` keeps, and packs it with the licences and
# INSTALL.md into <outdir>/aetherdesk-host-<version>-arm64-macos.tar.gz.
# Run from the repository root on a Mac (the release workflow does).
#
#   packaging/macos/release-tarball.sh <version> <outdir>
set -euo pipefail

version=${1:?usage: release-tarball.sh <version> <outdir>}
outdir=${2:?usage: release-tarball.sh <version> <outdir>}
name=aetherdesk-host-$version-arm64-macos
target=${CARGO_TARGET_DIR:-target}

cargo build --release --locked -p aetherdesk-mac-host --target aarch64-apple-darwin

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -p "$dir"
cp "$target/aarch64-apple-darwin/release/aether-agent" "$dir/"
# Ad hoc: no certificate here. `aether-agent install` signs its own copy again,
# with the user's identity when they have one.
codesign --force --sign - --identifier nl.stormlantern.aetherdesk.agent "$dir/aether-agent"
cp LICENSE-APACHE LICENSE-MIT packaging/macos/INSTALL.md "$dir/"

mkdir -p "$outdir"
tar -C "$stage" -czf "$outdir/$name.tar.gz" "$name"
(cd "$outdir" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$outdir/$name.tar.gz"
