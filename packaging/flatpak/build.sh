#!/usr/bin/env bash
# Builds the client's Flatpak and exports it as a single-file bundle,
# <outdir>/auroval-<version>-<arch>.flatpak, with its .sha256. Needs
# flatpak-builder and the Flathub remote (it installs the runtime and SDK
# for the user). Run from the repository root.
#
#   packaging/flatpak/build.sh <version> <outdir>
#
# Install the bundle with: flatpak install --user <bundle>
set -euo pipefail

version=${1:?usage: build.sh <version> <outdir>}
outdir=${2:?usage: build.sh <version> <outdir>}
id=nl.stormlantern.Auroval
arch=$(uname -m)
work=${FLATPAK_WORK:-flatpak-build}
name=auroval-$version-$arch.flatpak

flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
# No FUSE on CI runners; costs a little speed locally.
flatpak-builder --user --install-deps-from=flathub --force-clean --disable-rofiles-fuse \
  --state-dir="$work/state" --repo="$work/repo" \
  "$work/app" "packaging/flatpak/$id.yml"

mkdir -p "$outdir"
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
  "$work/repo" "$outdir/$name" "$id"
(cd "$outdir" && sha256sum "$name" > "$name.sha256")
echo "$outdir/$name"
