#!/usr/bin/env bash
# Builds the Mac host (Apple Silicon) as AetherDesk.app (app-bundle.sh),
# signs and notarizes it, and packs it two ways into <outdir>:
#
#   AetherDesk-<version>.pkg                       the installer (build-pkg.sh)
#   aetherdesk-host-<version>-arm64-macos.tar.gz   the app, licences and
#                                                  INSTALL.md, for Homebrew
#
# each with a .sha256. Run from the repository root on a Mac (the release
# workflow does).
#
#   packaging/macos/release.sh <version> <outdir>
#
# SIGNING=developer-id signs with the Developer ID Application and Installer
# certificates and notarizes (sign-notarize.sh and notarize.sh say what
# that needs), and needs SPARKLE_ED_PUBLIC_KEY; the release workflow always
# does. Otherwise it signs ad hoc and leaves the package unsigned, for
# trying the packaging locally.
set -euo pipefail

version=${1:?usage: release.sh <version> <outdir>}
outdir=${2:?usage: release.sh <version> <outdir>}
here=$(cd "$(dirname "$0")" && pwd)
name=aetherdesk-host-$version-arm64-macos
target=${CARGO_TARGET_DIR:-target}

cargo build --release --locked -p aetherdesk-mac-host --target aarch64-apple-darwin
bin=$target/aarch64-apple-darwin/release/aether-agent
built=$("$bin" version | cut -d' ' -f2)
[ "$built" = "$version" ] || { echo "error: built $built, asked for $version" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -p "$dir"
if [ "${SIGNING:-}" = developer-id ]; then
	RELEASE=1 "$here/app-bundle.sh" "$bin" "$dir" > /dev/null
	"$here/sign-notarize.sh" "$dir/AetherDesk.app"
else
	"$here/app-bundle.sh" "$bin" "$dir" > /dev/null
fi
cp LICENSE-APACHE LICENSE-MIT "$here/INSTALL.md" "$dir/"

mkdir -p "$outdir"
"$here/build-pkg.sh" "$dir/AetherDesk.app" "$outdir"
tar -C "$stage" -czf "$outdir/$name.tar.gz" "$name"
(
	cd "$outdir"
	for f in "$name.tar.gz" "AetherDesk-$version.pkg"; do
		shasum -a 256 "$f" > "$f.sha256"
	done
)
echo "$outdir/$name.tar.gz"
echo "$outdir/AetherDesk-$version.pkg"
