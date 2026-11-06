#!/usr/bin/env bash
# Builds the installer package <outdir>/AetherDesk-<version>.pkg from a
# built (and, for a release, signed and notarized) AetherDesk.app: a
# component package that puts the app in /Applications, with a preinstall
# that stops the host and a postinstall that sets it up and starts it
# (`aether-agent _postinstall`), wrapped by productbuild with welcome and
# conclusion pages. Run on a Mac from the repository root.
#
#   packaging/macos/build-pkg.sh <AetherDesk.app> <outdir>
#
# INSTALLER_IDENTITY   sign with this certificate (a part of its name, e.g.
#                      "Developer ID Installer"); unsigned without it.
# SIGNING=developer-id sign with "Developer ID Installer" (or
#                      INSTALLER_IDENTITY), then notarize and staple
#                      (notarize.sh says what that needs).
set -euo pipefail

app=${1:?usage: build-pkg.sh <AetherDesk.app> <outdir>}
outdir=${2:?usage: build-pkg.sh <AetherDesk.app> <outdir>}
here=$(cd "$(dirname "$0")" && pwd)
pkg_id=nl.stormlantern.aetherdesk.pkg
version=$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")
identity=${INSTALLER_IDENTITY:-}
if [ "${SIGNING:-}" = developer-id ]; then
	identity=${identity:-Developer ID Installer}
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root/Applications" "$work/scripts" "$work/packages"
ditto "$app" "$work/root/Applications/AetherDesk.app"
cp "$here/pkg/scripts/preinstall" "$here/pkg/scripts/postinstall" "$work/scripts/"
chmod 755 "$work/scripts/"*

# Always /Applications/AetherDesk.app: not relocated to a copy found
# elsewhere (the launchd jobs point here), and reinstalling or going back a
# version works too.
cat > "$work/component.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
	<dict>
		<key>BundleHasStrictIdentifier</key>
		<true/>
		<key>BundleIsRelocatable</key>
		<false/>
		<key>BundleIsVersionChecked</key>
		<false/>
		<key>BundleOverwriteAction</key>
		<string>upgrade</string>
		<key>RootRelativeBundlePath</key>
		<string>Applications/AetherDesk.app</string>
	</dict>
</array>
</plist>
EOF

pkgbuild --root "$work/root" --component-plist "$work/component.plist" \
	--scripts "$work/scripts" --identifier "$pkg_id" --version "$version" \
	--install-location / "$work/packages/AetherDesk-component.pkg"

sed -e "s/@VERSION@/$version/g" -e "s/@PKG_ID@/$pkg_id/g" \
	"$here/pkg/distribution.xml.in" > "$work/distribution.xml"

mkdir -p "$outdir"
out=$outdir/AetherDesk-$version.pkg
sign=()
[ -z "$identity" ] || sign=(--sign "$identity" --timestamp)
productbuild --distribution "$work/distribution.xml" --resources "$here/pkg/resources" \
	--package-path "$work/packages" ${sign[@]+"${sign[@]}"} "$out"

if [ -n "$identity" ]; then
	pkgutil --check-signature "$out"
fi
if [ "${SIGNING:-}" = developer-id ] && [ -z "${SKIP_NOTARIZE:-}" ]; then
	"$here/notarize.sh" "$out"
fi
echo "$out"
