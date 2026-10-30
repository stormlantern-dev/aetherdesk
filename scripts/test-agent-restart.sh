#!/usr/bin/env bash
# Run on Linux, against the real Mac (installed host, user logged in, client
# paired). Checks without logging out what a logout or login does to the
# user's agent: launchd stops it with SIGTERM.
#
#   1. a client connects with sound: the Mac gets muted;
#   2. the user's agent gets SIGTERM: it unmutes the Mac before it goes (#71);
#   3. launchd starts a new agent, which takes the session over (#18);
#   4. the client leaves: the Mac is unmuted again (#29).
#
#   MAC=<ssh host of the Mac> scripts/test-agent-restart.sh [client] [host name]
#
# The client runs without a window or sound device. Ending the session
# locks the Mac, as closing the client always does.
set -euo pipefail

: "${MAC:?set MAC to the ssh host of the Mac, as in ~/.ssh/config}"
CLIENT=${1:-target/release/aetherdesk}
HOST=${2:-}
TMP=$(mktemp -d)
LOG=$TMP/client.log
CLIENT_PID=

# Runs zsh code on the Mac (the login shell there may be nushell).
mac() { ssh -o BatchMode=yes "$MAC" zsh -s <<<"$1"; }
muted() { mac 'osascript -e "output muted of (get volume settings)"'; }
is_muted() { [ "$(muted)" = true ]; }
is_unmuted() { [ "$(muted)" = false ]; }
agent_pid() { mac 'pgrep -U "$(id -u)" -f "aether-agent agent" | head -n 1'; }
fail() {
	echo "FAIL: $*"
	exit 1
}
# wait_for <seconds> <what> <command...>
wait_for() {
	local secs=$1 what=$2
	shift 2
	local end=$((SECONDS + secs))
	while ((SECONDS < end)); do
		if "$@" >/dev/null 2>&1; then
			echo "ok: $what"
			return
		fi
		sleep 0.5
	done
	fail "$what (waited $secs s)"
}
cleanup() {
	[ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null || true
	echo "client log: $LOG"
}
trap cleanup EXIT

[ -x "$CLIENT" ] || fail "no client at $CLIENT (build it, or pass its path)"
mac 'grep -q "SIGTERM: stopping" "/Library/Application Support/AetherDesk/aether-agent"' ||
	fail "the installed host predates the SIGTERM handling: scripts/mac.zsh install on the Mac first"
[ "$(mac 'stat -f %Su /dev/console')" != root ] || fail "the Mac is at the login window; log in first"
is_unmuted || fail "the Mac is muted already; unmute it first, the test checks it comes back unmuted"
OLD=$(agent_pid)
[ -n "$OLD" ] || fail "no agent runs in the user's session"
echo "user's agent: pid $OLD"

# shellcheck disable=SC2086 # no host name: the only paired one
SDL_VIDEO_DRIVER=offscreen SDL_AUDIO_DRIVER=dummy "$CLIENT" $HOST >"$LOG" 2>&1 &
CLIENT_PID=$!
wait_for 20 "client connected" grep -q "connected:" "$LOG"
wait_for 10 "Mac muted for the session" is_muted

echo "SIGTERM to the user's agent (pid $OLD), as launchd does at logout"
mac "kill -TERM $OLD"
# launchd waits 10 s before starting the next one, so this comes first.
wait_for 5 "the agent unmuted the Mac before it went" is_unmuted

new_agent() { local p; p=$(agent_pid) && [ -n "$p" ] && [ "$p" != "$OLD" ]; }
wait_for 25 "launchd started a new agent" new_agent
wait_for 15 "the new agent took the session over (Mac muted again)" is_muted
grep -q "connection closed" "$LOG" && fail "the client lost the session"
echo "ok: the client kept the session"

kill "$CLIENT_PID"
wait "$CLIENT_PID" 2>/dev/null || true
CLIENT_PID=
wait_for 10 "the Mac is unmuted after the session" is_unmuted
echo "passed"
