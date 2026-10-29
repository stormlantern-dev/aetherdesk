#!/usr/bin/env bash
# Builds the Linux client and packs it with its .desktop entry, icons,
# licences and INSTALL.md into <outdir>/aetherdesk-<version>-<arch>-linux.tar.gz.
# Run from the repository root (the release workflow runs it in the devcontainer image).
#
#   packaging/linux/release-tarball.sh <version> <outdir>
set -euo pipefail

version=${1:?usage: release-tarball.sh <version> <outdir>}
outdir=${2:?usage: release-tarball.sh <version> <outdir>}
arch=$(uname -m)
id=dev.stormlantern.AetherDesk
name=aetherdesk-$version-$arch-linux
target=${CARGO_TARGET_DIR:-target}

cargo build --release --locked -p aetherdesk-linux-client --bin aetherdesk

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
install -Dm755 "$target/release/aetherdesk" -t "$dir/"
install -Dm644 "packaging/linux/$id.desktop" -t "$dir/"
install -Dm644 packaging/linux/icons/* -t "$dir/icons/"
install -Dm644 LICENSE-APACHE LICENSE-MIT assets/fonts/Inter-OFL.txt packaging/linux/INSTALL.md -t "$dir/"

mkdir -p "$outdir"
tar -C "$stage" --owner=0 --group=0 -czf "$outdir/$name.tar.gz" "$name"
(cd "$outdir" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$outdir/$name.tar.gz"
