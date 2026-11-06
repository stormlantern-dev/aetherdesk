#!/usr/bin/env bash
# Signs AetherDesk.app with the Developer ID Application certificate
# (hardened runtime, secure timestamp, inside out: sign-app.sh), checks it,
# then notarizes and staples it (notarize.sh). Fails unless Apple accepts it.
#
#   packaging/macos/sign-notarize.sh <AetherDesk.app>
#
# Signing: the certificate must be in a keychain on the search list.
# SIGN_IDENTITY picks it (a part of its name; default "Developer ID
# Application", enough when there is only one).
#
# Notarizing: see notarize.sh for the variables. SKIP_NOTARIZE=1 signs only,
# for trying the hardened runtime locally.
set -euo pipefail

app=${1:?usage: sign-notarize.sh <AetherDesk.app>}
here=$(cd "$(dirname "$0")" && pwd)
identity=${SIGN_IDENTITY:-Developer ID Application}

"$here/sign-app.sh" "$app" "$identity"
codesign --verify --deep --strict --verbose=2 "$app"
info=$(codesign -dv --verbose=4 "$app" 2>&1)
grep -q '^Authority=Developer ID Application' <<<"$info" ||
	{ echo "not signed with a Developer ID Application certificate"; exit 1; }
grep -q 'flags=.*runtime' <<<"$info" || { echo "hardened runtime missing"; exit 1; }
echo "signed: $(sed -n 's/^Identifier=//p' <<<"$info"), hardened runtime"

if [ -n "${SKIP_NOTARIZE:-}" ]; then
	echo "not notarized (SKIP_NOTARIZE)"
	exit 0
fi
"$here/notarize.sh" "$app"
