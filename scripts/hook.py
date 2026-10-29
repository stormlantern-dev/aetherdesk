#!/usr/bin/env python3
"""Send commands to a client built with `--features test-hooks`.

    AETHERDESK_TEST_SOCKET=/tmp/ad.sock aetherdesk <host> &
    scripts/hook.py /tmp/ad.sock state
    scripts/hook.py /tmp/ad.sock key Right_Ctrl "snapshot /tmp/w.png" state

Each argument is one command (see crates/linux-client/src/testhooks.rs);
the answers are printed one per line. Exits 1 if an answer starts with
"error". Waits up to 10 s for the socket to appear.
"""
import socket
import sys
import time


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    path, commands = sys.argv[1], sys.argv[2:]
    deadline = time.monotonic() + 10
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            break
        except OSError:
            if time.monotonic() > deadline:
                print(f"error: no test socket at {path}", file=sys.stderr)
                return 1
            time.sleep(0.2)
    failed = False
    with s, s.makefile("rw") as f:
        for command in commands:
            f.write(command + "\n")
            f.flush()
            answer = f.readline().rstrip("\n")
            print(answer)
            failed |= answer.startswith("error")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
