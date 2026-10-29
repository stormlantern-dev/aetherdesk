#!/usr/bin/env bash
# Fills in the Homebrew casks for a release from its .sha256 files and writes
# them into a checkout of the tap (stormlantern-dev/homebrew-tap). Doesn't
# commit or push: look at the diff, then do that yourself (docs/RELEASING.md).
#
#   packaging/homebrew/casks.sh <version> <tap-checkout>
set -euo pipefail

version=${1:?usage: casks.sh <version> <tap-checkout>}
tap=${2:?usage: casks.sh <version> <tap-checkout>}
here=$(cd "$(dirname "$0")" && pwd)
repo=stormlantern-dev/aetherdesk

sums=$(mktemp -d)
trap 'rm -rf "$sums"' EXIT
gh release download "v$version" --repo "$repo" --pattern '*.sha256' --dir "$sums"
sum() { cut -d' ' -f1 "$sums/$1.tar.gz.sha256"; }
linux=$(sum "aetherdesk-$version-x86_64-linux")
mac=$(sum "aetherdesk-host-$version-arm64-macos")

mkdir -p "$tap/Casks"
fill() {
  sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA256_LINUX_X86_64@/$linux/g" \
    -e "s/@SHA256_MACOS_ARM64@/$mac/g" "$here/$1.rb.in" >"$tap/Casks/$1.rb"
  echo "$tap/Casks/$1.rb"
}
fill aetherdesk
fill aetherdesk-host
