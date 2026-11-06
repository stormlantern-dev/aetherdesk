#!/usr/bin/env bash
# Downloads the pinned Sparkle 2 release from its official GitHub release,
# checks its SHA-256 and unpacks it into <dir>/Sparkle-<version> (kept there
# as a cache). Prints that directory: Sparkle.framework, and bin/ with
# sign_update and generate_keys.
#
#   packaging/macos/fetch-sparkle.sh <dir>
#
# To move to a newer Sparkle: change both lines below, in their own PR. The
# digest is on the release page's asset, or:
#   gh api repos/sparkle-project/Sparkle/releases/tags/<version> --jq '.assets[].digest'
set -euo pipefail

SPARKLE_VERSION=2.10.1
SPARKLE_SHA256=1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27

dir=${1:?usage: fetch-sparkle.sh <dir>}
out=$dir/Sparkle-$SPARKLE_VERSION
if [ -f "$out/.verified" ]; then
	echo "$out"
	exit 0
fi

mkdir -p "$dir"
archive=$dir/Sparkle-$SPARKLE_VERSION.tar.xz
url=https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VERSION/Sparkle-$SPARKLE_VERSION.tar.xz
curl -fsSL --retry 3 -o "$archive" "$url"
actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
if [ "$actual" != "$SPARKLE_SHA256" ]; then
	rm -f "$archive"
	echo "Sparkle $SPARKLE_VERSION: SHA-256 is $actual, expected $SPARKLE_SHA256" >&2
	exit 1
fi
rm -rf "$out"
mkdir -p "$out"
tar -xJf "$archive" -C "$out" ./Sparkle.framework ./bin ./LICENSE
rm -f "$archive"
touch "$out/.verified"
echo "$out"
