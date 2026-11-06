#!/usr/bin/env bash
# Notarizes an app bundle or an installer package and staples the ticket to
# it, so Gatekeeper accepts it offline too. Fails unless Apple accepts it.
#
#   packaging/macos/notarize.sh <AetherDesk.app | something.pkg>
#
# One of:
#   NOTARY_PROFILE=<name>   a profile saved with `xcrun notarytool store-credentials`
#   NOTARY_KEY=<.p8 path> NOTARY_KEY_ID=<id> NOTARY_ISSUER=<issuer id>
#                           an App Store Connect API key (what CI uses)
set -euo pipefail

item=${1:?usage: notarize.sh <app or pkg>}
if [ -n "${NOTARY_PROFILE:-}" ]; then
	auth=(--keychain-profile "$NOTARY_PROFILE")
elif [ -n "${NOTARY_KEY:-}" ]; then
	auth=(--key "$NOTARY_KEY" --key-id "${NOTARY_KEY_ID:?}" --issuer "${NOTARY_ISSUER:?}")
else
	echo "set NOTARY_PROFILE, or NOTARY_KEY, NOTARY_KEY_ID and NOTARY_ISSUER (or SKIP_NOTARIZE=1)"
	exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case $item in
*.pkg) submit=$item ;;
*)
	# notarytool takes a zip, a pkg or a dmg, not a bundle.
	submit=$work/$(basename "$item").zip
	ditto -c -k --keepParent "$item" "$submit"
	;;
esac
echo "notarizing $(basename "$item"): waiting for Apple (usually minutes; a new account's first can take an hour or more)"
[ -n "${NOTARY_PROFILE:-}" ] &&
	echo "  progress: xcrun notarytool history --keychain-profile $NOTARY_PROFILE"
out=$(xcrun notarytool submit "$submit" "${auth[@]}" --wait --output-format json)
echo "$out"
id=$(plutil -extract id raw -o - - <<<"$out")
status=$(plutil -extract status raw -o - - <<<"$out")
if [ "$status" != Accepted ]; then
	xcrun notarytool log "$id" "${auth[@]}" || true
	echo "notarization: $status"
	exit 1
fi
echo "notarized ($id)"
xcrun stapler staple "$item"
xcrun stapler validate "$item"
