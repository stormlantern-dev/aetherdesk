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
# Never leave a fake host running, whatever happens.
trap 'kill $(jobs -p) 2>/dev/null || true' EXIT
# Each client gets its own config directory: its own identity and hosts.
client() { local who=$1; shift; XDG_CONFIG_HOME="$log/$who" timeout 20 "$B/aetherdesk" "$@"; }

"$B/examples/fake_host" --port 4511 --pair-code 123456 --frames 150 >"$log/host.log" 2>&1 &
host1=$!
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
wait $host1
shown=$(grep -c "fps shown" "$log/client.log" || true)
[ "$shown" -ge 3 ] || fail "expected at least 3 seconds of video, got $shown"

# Something else on the paired host's address (a new identity): the client says no.
"$B/examples/fake_host" --port 4512 --frames 30 >"$log/host2.log" 2>&1 &
host2=$!
sleep 1
sed 's/127.0.0.1:4511 fake/127.0.0.1:4512 imposter/' "$log/a/aetherdesk/hosts" >"$log/a/aetherdesk/hosts.new"
mv "$log/a/aetherdesk/hosts.new" "$log/a/aetherdesk/hosts"
if client a imposter 2>"$log/imposter.log"; then
  fail "the client accepted a host with a different certificate"
fi
kill $host2 2>/dev/null || true
grep -q "different certificate" "$log/imposter.log" || fail "no 'different certificate' message"

# The host goes away mid-session ("shutting down") and comes back with the
# same identity: the client reconnects by itself and gets video again.
state="$log/host-state"; mkdir -p "$state"
"$B/examples/fake_host" --port 4513 --state "$state" --pair-code 654321 >"$log/host3.log" 2>&1 &
host3=$!
sleep 1
client c pair 127.0.0.1:4513 --code 654321 --name restarting >"$log/pair3.log" 2>&1 || fail "pairing with host 3 failed"
kill $host3; wait $host3 2>/dev/null || true
"$B/examples/fake_host" --port 4513 --state "$state" --frames 60 --close-code 4 >"$log/host3a.log" 2>&1 &
host3a=$!
sleep 1
client c restarting >"$log/reconnect.log" 2>&1 &
reconnecting=$!
wait $host3a || true # 60 frames, then "shutting down"
"$B/examples/fake_host" --port 4513 --state "$state" --frames 90 >"$log/host3b.log" 2>&1 &
host3b=$!
wait $reconnecting || true
wait $host3b || true
grep -q "connection lost: the Mac host is shutting down" "$log/reconnect.log" || fail "no 'connection lost' after the host shut down"
grep -q "^reconnected" "$log/reconnect.log" || fail "the client didn't reconnect"
grep -q "disconnected" "$log/host3b.log" || fail "no session with the restarted host"

echo "e2e OK: pairing (wrong code refused), $shown seconds of video, unpaired client and foreign host refused, reconnect after a host restart"
