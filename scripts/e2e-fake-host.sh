#!/usr/bin/env bash
# End-to-end check of the Linux client against the fake host: pairing (right
# and wrong code), video decode and display (SDL offscreen), an unpaired
# client refused by the host, and a host with a different certificate
# refused by the client.
# Runs inside the devcontainer image; expects built debug binaries.
set -euo pipefail
B=${CARGO_TARGET_DIR:-target}/debug
export SDL_VIDEO_DRIVER=offscreen
log=${E2E_LOG:-$(mktemp -d)}
fail() { echo "FAIL: $*"; for f in "$log"/*.log; do echo "--- $f"; cat "$f"; done; exit 1; }
# Each client gets its own config directory: its own identity and hosts.
client() { local who=$1; shift; XDG_CONFIG_HOME="$log/$who" timeout 20 "$B/aetherdesk" "$@"; }

"$B/examples/fake_host" --port 4511 --pair-code 123456 --frames 150 >"$log/host.log" 2>&1 &
sleep 1

# A wrong code is refused.
if client a pair 127.0.0.1:4511 --code 000000 2>"$log/wrong-code.log"; then
  fail "pairing with a wrong code succeeded"
fi
grep -q "wrong code" "$log/wrong-code.log" || fail "no 'wrong code' message"

# The right code pairs, under the name we give.
client a pair 127.0.0.1:4511 --code 123-456 --name fake >"$log/pair.log" 2>&1 || fail "pairing failed"
client a hosts | grep -q "fake" || fail "paired host not listed"

# Another client, pinned to the same host but never paired: the host says no.
mkdir -p "$log/b/aetherdesk"
cp "$log/a/aetherdesk/hosts" "$log/b/aetherdesk/hosts"
if client b fake 2>"$log/unpaired.log"; then
  fail "an unpaired client was let in"
fi
grep -q "doesn't know this client" "$log/unpaired.log" || fail "no 'not paired' message"

# The paired client gets video.
client a fake | tee "$log/client.log"
wait
shown=$(grep -c "fps shown" "$log/client.log" || true)
[ "$shown" -ge 3 ] || fail "expected at least 3 seconds of video, got $shown"

# Something else on the paired host's address (a new identity): the client says no.
"$B/examples/fake_host" --port 4512 --frames 30 >"$log/host2.log" 2>&1 &
sleep 1
sed 's/127.0.0.1:4511 fake/127.0.0.1:4512 imposter/' "$log/a/aetherdesk/hosts" >"$log/a/aetherdesk/hosts.new"
mv "$log/a/aetherdesk/hosts.new" "$log/a/aetherdesk/hosts"
if client a imposter 2>"$log/imposter.log"; then
  fail "the client accepted a host with a different certificate"
fi
kill %1 2>/dev/null || true
grep -q "different certificate" "$log/imposter.log" || fail "no 'different certificate' message"

echo "e2e OK: pairing (wrong code refused), $shown seconds of video, unpaired client and foreign host refused"
