#!/usr/bin/env bash
# Writes Sparkle's appcast for one release: an item for the .pkg with its
# EdDSA signature and the release's notes, then signs the feed itself (the
# app refuses an unsigned or altered feed: SURequireSignedFeed). The release
# workflow attaches it to the release; the Appcast workflow publishes it.
#
#   packaging/macos/appcast.sh <AetherDesk-<version>.pkg> <out.xml>
#
# Version, build number and minimum macOS come from the app inside the
# package, so the feed says what the package installs.
#
# SPARKLE_ED_KEY_FILE  the private EdDSA key (from generate_keys -x), a file
# SPARKLE_BIN          Sparkle's bin/ (default: from fetch-sparkle.sh)
# NOTES_FILE           release notes in Markdown (default: the GitHub
#                      release's body, through gh)
set -euo pipefail

pkg=${1:?usage: appcast.sh <pkg> <out.xml>}
out=${2:?usage: appcast.sh <pkg> <out.xml>}
here=$(cd "$(dirname "$0")" && pwd)
key=${SPARKLE_ED_KEY_FILE:?set SPARKLE_ED_KEY_FILE to the private EdDSA key: see docs/RELEASING.md}
bin=${SPARKLE_BIN:-$("$here/fetch-sparkle.sh" "${CARGO_TARGET_DIR:-target}/sparkle")/bin}
repo=stormlantern-dev/aetherdesk

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pkgutil --expand-full "$pkg" "$work/pkg"
plist=$work/pkg/AetherDesk-component.pkg/Payload/Applications/AetherDesk.app/Contents/Info.plist
plutil -extract SUPublicEDKey raw -o - "$plist" > /dev/null ||
	{ echo "error: the app in $pkg has no SUPublicEDKey, so it can't check updates" >&2; exit 1; }
version=$(plutil -extract CFBundleShortVersionString raw -o - "$plist")
build=$(plutil -extract CFBundleVersion raw -o - "$plist")
minimum=$(plutil -extract LSMinimumSystemVersion raw -o - "$plist")
[ "$(basename "$pkg")" = "AetherDesk-$version.pkg" ] ||
	{ echo "error: $pkg isn't the package of AetherDesk $version" >&2; exit 1; }
url=https://github.com/$repo/releases/download/v$version/AetherDesk-$version.pkg

signature=$("$bin/sign_update" -p --ed-key-file "$key" "$pkg")
grep -qE '^[A-Za-z0-9+/]{86}==$' <<<"$signature" ||
	{ echo "error: sign_update said: $signature" >&2; exit 1; }
length=$(stat -f %z "$pkg")

if [ -n "${NOTES_FILE:-}" ]; then
	notes=$(cat "$NOTES_FILE")
else
	notes=$(gh release view "v$version" --repo "$repo" --json body --jq .body)
fi
# Inside CDATA; a literal ]]> would end it.
notes=${notes//]]>/]]]]><![CDATA[>}

cat > "$out" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>AetherDesk host</title>
    <link>https://github.com/$repo</link>
    <description>Updates of the AetherDesk host for macOS</description>
    <language>en</language>
    <item>
      <title>AetherDesk $version</title>
      <pubDate>$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')</pubDate>
      <sparkle:version>$build</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>$minimum</sparkle:minimumSystemVersion>
      <sparkle:fullReleaseNotesLink>https://github.com/$repo/releases/tag/v$version</sparkle:fullReleaseNotesLink>
      <description sparkle:format="markdown"><![CDATA[$notes]]></description>
      <enclosure url="$url" sparkle:installationType="package" type="application/octet-stream" sparkle:edSignature="$signature" length="$length"/>
    </item>
  </channel>
</rss>
EOF
# Embeds the feed's signature in the file.
"$bin/sign_update" --ed-key-file "$key" "$out"
echo "$out"
