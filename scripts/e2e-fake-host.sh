#!/usr/bin/env bash
# End-to-end check of the Linux client against the fake host: handshake,
# video decode and display (SDL offscreen), and token rejection.
# Runs inside the devcontainer image; expects built debug binaries.
set -euo pipefail
B=${CARGO_TARGET_DIR:-target}/debug
export SDL_VIDEO_DRIVER=offscreen
log=$(mktemp -d)

"$B/examples/fake_host" --port 4511 --token secret --frames 150 >"$log/host.log" 2>&1 &
sleep 1
timeout 20 "$B/aetherdesk" 127.0.0.1:4511 --token secret | tee "$log/client.log"
wait
shown=$(grep -c "fps shown" "$log/client.log" || true)
if [ "$shown" -lt 3 ]; then
  echo "FAIL: expected at least 3 seconds of video, got $shown"; cat "$log/host.log"; exit 1
fi

"$B/examples/fake_host" --port 4512 --token secret --frames 30 >"$log/host2.log" 2>&1 &
sleep 1
if timeout 10 "$B/aetherdesk" 127.0.0.1:4512 --token wrong 2>"$log/client2.log"; then
  echo "FAIL: client with the wrong token was accepted"; exit 1
fi
kill %1 2>/dev/null || true
grep -q "wrong token" "$log/client2.log" || { echo "FAIL: no 'wrong token' message"; cat "$log/client2.log"; exit 1; }
echo "e2e OK: $shown seconds of video, wrong token refused"
