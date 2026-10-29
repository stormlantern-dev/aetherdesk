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
grep -q "connected: .*, H.264 4:2:0" "$log/client.log" || fail "the client didn't report the agreed video format"

# HEVC: a client that offers it (forced here; normally only with a GPU to
# decode on) gets it from a host that can send it, and decodes it.
"$B/examples/fake_host" --port 4517 --pair-code 777888 --frames 150 >"$log/host7.log" 2>&1 &
host7=$!
sleep 1
client g pair 127.0.0.1:4517 --code 777888 --name hevc >"$log/pair7.log" 2>&1 || fail "pairing with host 7 failed"
AETHERDESK_CODECS=hevc,h264 client g hevc >"$log/hevc.log" 2>&1 || true
wait $host7 || true
grep -q "connected: .*, HEVC 4:2:0" "$log/hevc.log" || fail "no HEVC session: $(grep connected "$log/hevc.log")"
hevc_shown=$(grep -c "fps shown" "$log/hevc.log" || true)
[ "$hevc_shown" -ge 3 ] || fail "expected at least 3 seconds of HEVC video, got $hevc_shown"

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

# Driving the window through the test hooks (a client built with
# --features test-hooks): Right Ctrl releases the grab and shows the LOCAL
# badge, a second one grabs again; a snapshot captures the window.
if grep -aq AETHERDESK_TEST_SOCKET "$B/aetherdesk"; then
  "$B/examples/fake_host" --port 4514 --pair-code 111222 --frames 900 >"$log/host4.log" 2>&1 &
  host4=$!
  sleep 1
  client d pair 127.0.0.1:4514 --code 111222 --name hooks >"$log/pair4.log" 2>&1 || fail "pairing with host 4 failed"
  sock="$log/hooks.sock"
  AETHERDESK_TEST_SOCKET="$sock" XDG_CONFIG_HOME="$log/d" timeout 20 "$B/aetherdesk" hooks >"$log/hooks-client.log" 2>&1 &
  hooked=$!
  hook() { scripts/hook.py "$sock" "$@"; }
  hook state | grep -q '"grabbed":true' || fail "not grabbed at the start: $(hook state)"
  hook state | grep -q '"pointer":"local"' || fail "the host's pointer shape wasn't taken up: $(hook state)"
  hook "key Right_Ctrl" >/dev/null
  hook state | grep -q '"grabbed":false' || fail "Right Ctrl didn't release: $(hook state)"
  hook state | grep -q 'LOCAL' || fail "no LOCAL badge: $(hook state)"
  hook "snapshot $log/local.png" | grep -q '^ok ' || fail "snapshot failed"
  [ -s "$log/local.png" ] || fail "empty snapshot"
  hook "key Right_Ctrl" >/dev/null
  hook state | grep -q '"grabbed":true' || fail "Right Ctrl didn't grab again: $(hook state)"
  hook state | grep -q -- '-> Mac' || fail "no grab toast: $(hook state)"
  # Quitting must close the connection, not just go quiet: the Mac locks
  # right away only when the client says goodbye.
  kill -TERM $hooked
  wait $hooked || true
  wait $host4 || true
  grep -q "the client said goodbye" "$log/host4.log" || fail "the client quit without closing the connection: $(tail -3 "$log/host4.log")"

  # Clipboard: a copy here reaches the host once; the host's copy (it adds
  # " ✓") lands on our clipboard intact and doesn't bounce back; a copy over
  # the cap is skipped; --no-clipboard sends nothing; no content in the logs.
  "$B/examples/fake_host" --port 4516 --pair-code 555666 >"$log/host6.log" 2>&1 &
  host6=$!
  sleep 1
  client f pair 127.0.0.1:4516 --code 555666 --name clip >"$log/pair6.log" 2>&1 || fail "pairing with host 6 failed"
  sock6="$log/clip.sock"
  AETHERDESK_TEST_SOCKET="$sock6" XDG_CONFIG_HOME="$log/f" timeout 20 "$B/aetherdesk" clip >"$log/clip-client.log" 2>&1 &
  clipped=$!
  h6() { scripts/hook.py "$sock6" "$@"; }
  h6 state >/dev/null
  h6 'copy héllo 👋 wörld 世界\nline 2' >/dev/null
  sleep 1
  h6 paste | grep -qF '"héllo 👋 wörld 世界\u000aline 2 ✓"' || fail "the host's copy didn't arrive intact: $(h6 paste)"
  h6 "copybig 1100000" >/dev/null
  sleep 0.5
  [ "$(grep -c "clipboard: skipped a copy of 1100000 bytes" "$log/clip-client.log")" = 1 ] || fail "expected one skip line for a copy over the cap"
  kill -TERM $clipped; wait $clipped || true
  sent=$(grep -c "clipboard from client" "$log/host6.log" || true)
  [ "$sent" = 1 ] || fail "expected 1 copy at the host, got $sent (echo?)"
  AETHERDESK_TEST_SOCKET="$sock6" XDG_CONFIG_HOME="$log/f" timeout 20 "$B/aetherdesk" clip --no-clipboard >"$log/noclip-client.log" 2>&1 &
  clipped=$!
  h6 state >/dev/null
  h6 'copy not for the Mac' >/dev/null
  sleep 1
  kill -TERM $clipped; wait $clipped || true
  kill $host6 2>/dev/null || true; wait $host6 2>/dev/null || true
  grep -q "clipboard sync off" "$log/host6.log" || fail "--no-clipboard didn't tell the host"
  sent=$(grep -c "clipboard from client" "$log/host6.log" || true)
  [ "$sent" = 1 ] || fail "--no-clipboard still sent a copy"
  if grep -q -E "héllo|not for the Mac" "$log/host6.log" "$log/clip-client.log" "$log/noclip-client.log"; then
    fail "clipboard content in a log"
  fi
  hooks_ok=", window driven through the test hooks, clipboard both ways"
else
  hooks_ok=" (client built without test-hooks: window checks skipped)"
fi

# Discovery: pair by the name a host announces on the LAN (mDNS), then
# connect after its remembered address went stale: the client finds it
# again by its certificate and remembers the new address.
lan_name="e2e fake $$"
"$B/examples/fake_host" --port 4515 --pair-code 333444 --frames 60 --announce "$lan_name" --state "$log/host5" >"$log/host5.log" 2>&1 &
host5=$!
sleep 1
client e pair "$lan_name" --code 333444 >"$log/pair5.log" 2>&1 || fail "pairing by LAN name failed"
grep -q "found $lan_name on the LAN" "$log/pair5.log" || fail "pairing didn't find the host on the LAN"
sed -i -E 's/^([0-9a-f]{64}) [^ ]+ /\1 192.0.2.1:4515 /' "$log/e/aetherdesk/hosts"
client e >"$log/moved.log" 2>&1 || true
kill $host5 2>/dev/null || true; wait $host5 || true
grep -q "found .* on the LAN at .* (was 192.0.2.1:4515)" "$log/moved.log" || fail "the client didn't find the moved host"
grep -q "disconnected" "$log/host5.log" || fail "no session with the moved host"
if grep -q " 192.0.2.1:4515 " "$log/e/aetherdesk/hosts"; then fail "the new address wasn't remembered"; fi

echo "e2e OK: pairing (wrong code refused), $shown seconds of video (H.264) and $hevc_shown (HEVC), unpaired client and foreign host refused, reconnect after a host restart${hooks_ok}, found on the LAN by name and after moving"
