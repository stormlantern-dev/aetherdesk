#!/usr/bin/env bash
# CycloneDX SBOM of one crate's Rust dependencies for one target, for the
# release (needs cargo-cyclonedx). Native libraries (SDL3, ffmpeg in the
# Flatpak) and the Mac audio driver aren't Rust crates and aren't in it.
#
#   packaging/sbom.sh <crate dir under crates/> <target triple> <output file>
set -euo pipefail
crate=$1 target=$2 out=$3
name=aetherdesk-sbom-$$
cargo cyclonedx --format json --spec-version 1.5 --target "$target" \
  --override-filename "$name" --quiet
mv "crates/$crate/$name.json" "$out"
rm -f crates/*/"$name".json
