#!/usr/bin/env bash
# Signs aether-agent with the Developer ID Application certificate (hardened
# runtime, secure timestamp, the fixed identifier `aether-agent install`
# keeps) and notarizes it. Fails unless Apple accepts it.
#
#   packaging/macos/sign-notarize.sh <path to aether-agent>
#
# Signing: the certificate must be in a keychain on the search list.
# SIGN_IDENTITY picks it (a part of its name; default "Developer ID
# Application", enough when there is only one).
#
# Notarizing, one of:
#   NOTARY_PROFILE=<name>   a profile saved with `xcrun notarytool store-credentials`
#   NOTARY_KEY=<.p8 path> NOTARY_KEY_ID=<id> NOTARY_ISSUER=<issuer id>
#                           an App Store Connect API key (what CI uses)
# SKIP_NOTARIZE=1 signs only, for trying the hardened runtime locally.
set -euo pipefail

bin=${1:?usage: sign-notarize.sh <path to aether-agent>}
identity=${SIGN_IDENTITY:-Developer ID Application}
identifier=nl.stormlantern.aetherdesk.agent

codesign --force --sign "$identity" --identifier "$identifier" \
	--options runtime --timestamp "$bin"
codesign --verify --strict --verbose=2 "$bin"
info=$(codesign -dv --verbose=4 "$bin" 2>&1)
grep -q '^Authority=Developer ID Application' <<<"$info" ||
	{ echo "not signed with a Developer ID Application certificate"; exit 1; }
grep -q 'flags=.*runtime' <<<"$info" || { echo "hardened runtime missing"; exit 1; }
echo "signed: $identifier, hardened runtime"

if [ -n "${SKIP_NOTARIZE:-}" ]; then
	echo "not notarized (SKIP_NOTARIZE)"
	exit 0
fi
if [ -n "${NOTARY_PROFILE:-}" ]; then
	auth=(--keychain-profile "$NOTARY_PROFILE")
elif [ -n "${NOTARY_KEY:-}" ]; then
	auth=(--key "$NOTARY_KEY" --key-id "${NOTARY_KEY_ID:?}" --issuer "${NOTARY_ISSUER:?}")
else
	echo "set NOTARY_PROFILE, or NOTARY_KEY, NOTARY_KEY_ID and NOTARY_ISSUER (or SKIP_NOTARIZE=1)"
	exit 1
fi

# notarytool takes a zip, not a bare binary. A bare binary can't carry a
# stapled ticket: Gatekeeper looks the notarization up online instead.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ditto -c -k --keepParent "$bin" "$work/aether-agent.zip"
out=$(xcrun notarytool submit "$work/aether-agent.zip" "${auth[@]}" --wait --output-format json)
echo "$out"
id=$(plutil -extract id raw -o - - <<<"$out")
status=$(plutil -extract status raw -o - - <<<"$out")
if [ "$status" != Accepted ]; then
	xcrun notarytool log "$id" "${auth[@]}" || true
	echo "notarization: $status"
	exit 1
fi
echo "notarized ($id)"
