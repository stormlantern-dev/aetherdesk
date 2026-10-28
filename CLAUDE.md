# AetherDesk — notes for Claude

Remote desktop: a **macOS host** streams its screen to a **Linux (Wayland) client**. Plan and milestones: [docs/MVP-PLAN.md](docs/MVP-PLAN.md). Each milestone has an issue with a checkbox test plan: M0 #3, M1 #7, M2 #18, M3 #13.

Two machines work on this repo, each with its own Claude session. Check which one you're on with `uname`:

- **Linux** (CachyOS + Hyprland, NVIDIA): planning, PRs, CI, client development in the devcontainer.
- **Mac** (MacBook Pro M3, macOS Tahoe, FileVault on): runs the host. Usually your job there is to **test a PR on the Mac and report back** — see below.

The user shouldn't have to relay long stories between the two. Results go to GitHub (PR or issue comments), where the other side reads them.

## Workflow (both machines)

- `main` is protected: never commit to it. Branch, push, open a PR. CI must be green (`workspace (Linux)`, `workspace (macOS)`, `spike-m0 (macOS)`).
- PR body: what changed and why, then `Refers #N`. Never `Closes`/`Fixes`/`Resolves`, and no test plan in the PR body.
- Test plans live on the **issue** as checkboxes. Close an issue by hand, only after a verification comment that covers every item (`[x]` verified, `[~]` partial, `[ ]` deferred, each with a one-line note).
- Merging is the user's call.

## On the Mac: testing a PR

The user's shell is **nushell** here too: give them commands in nushell syntax (`$"gui/(id -u)"`, not `gui/$UID`). The scripts in `scripts/*.zsh` are zsh and run fine from nushell. The host builds natively (a container can't use ScreenCaptureKit or VideoToolbox). Don't run `sudo` yourself; give the user the command.

1. **Get the PR and check the machine:**
   ```zsh
   scripts/mac.zsh pr <number>
   ```
   This switches to the PR's branch and runs `aether-agent doctor` (permissions, display size, IP addresses). If a permission is MISSING, tell the user exactly what to grant — System Settings → Privacy & Security → *Screen & System Audio Recording* and/or *Accessibility*, for the terminal app the agent runs from — and that the terminal must be restarted afterwards. You can't click those yourself.

2. **Run the host** in the background so the user can connect from Linux:
   ```zsh
   scripts/mac.zsh serve
   ```
   `serve` only lets in clients paired with *it* (it keeps its own pairings, apart from the installed host). If the Linux side isn't paired with `serve` yet, run `aether-agent pair --serve` (via `cargo run --release -p aetherdesk-mac-host -- pair --serve`) and tell the user the code.

3. **Tell the user what to try**, based on the PR description and the issue's test plan, and wait for their observations. Ask short questions; don't make them write essays.

4. **Report on the PR** with `gh pr comment <number>`: what was tested on the Mac, what worked, what didn't, and relevant host output (`serve` log lines, `doctor` output, errors). **Never include pairing codes or key files** — the repo is public. Mention which issue test-plan items the results cover, but leave ticking the boxes to the verification on the issue.

**Getting output to GitHub without typing:** `scripts/report.nu <issue-or-PR> logs` posts the last lines of the host logs, `scripts/report.nu 3 m0` the newest M0 spike run, and `<command> | scripts/report.nu <n>` anything else. `--dry-run` shows the comment first. Point the user at this instead of asking them to copy or retype output.

Other host commands: `scripts/mac.zsh record --seconds 10` (capture + encode to `/tmp/aetherdesk.h264`, no network), `scripts/mac.zsh update [branch]`.

### Installed host (M2: daemon + agents)

From M2 on, the host also runs **installed**: a root daemon (`aether-agent daemon`, the network side) plus an agent per GUI session (`aether-agent agent`, capture and input), started by launchd. That's what makes the locked and logged-out cases work.

- `scripts/mac.zsh install` builds, signs with the self-signed identity (`AETHERDESK_IDENTITY`, default "AetherDesk Dev") and installs. It needs `sudo`, so the **user** runs it, not you. The host's identity (certificate and key) and its paired clients live root-only in `/Library/Application Support/AetherDesk/`; `uninstall` removes them, so clients have to pair again after a reinstall from scratch.
- **Pairing (M3):** `scripts/mac.zsh pair` (sudo) shows a 6-digit code for 2 minutes; the user runs `aetherdesk pair <mac-ip>` on Linux and types it. `scripts/mac.zsh clients` lists paired clients, `scripts/mac.zsh unpair <name>` forgets one.
- After the first install, the user grants **Screen Recording** and **Accessibility** to `/Library/Application Support/AetherDesk/aether-agent` (not to the terminal), once.
- `scripts/mac.zsh status`: whether the daemon and agent run, plus the last log lines. `scripts/mac.zsh logs` follows the logs in `/Library/Logs/AetherDesk/`. Paste relevant lines into your report (never a pairing code).
- While installed, the daemon holds UDP 4500, so `serve` can't bind. Use `serve --port 4501`, or `scripts/mac.zsh uninstall` first.
- After testing a PR that changes the host, re-run `scripts/mac.zsh install` so the installed copy matches the branch.

## On Linux

Shell is **nushell** — give the user commands in nushell syntax. Never run `sudo`; give the user the command instead.

Rust runs in the devcontainer (Podman):

```nu
devcontainer up --docker-path podman --workspace-folder .
devcontainer exec --docker-path podman --workspace-folder . nu -c 'cargo test --workspace'
devcontainer exec --docker-path podman --workspace-folder . nu      # interactive shell
```

The container has nushell too, so the same syntax works inside and out (`scripts/*.nu` run there as well). Its login shell stays bash, because the devcontainer CLI reads the environment through it; `bash -lc '…'` keeps working.

- Type-check the Mac host from Linux: `cargo clippy -p aetherdesk-mac-host --all-targets --target aarch64-apple-darwin` (works in the devcontainer image; nothing is linked).
- Test the client without a Mac: `scripts/e2e-fake-host.sh` (CI runs it too), or `cargo run --example fake_host`, then `aetherdesk pair 127.0.0.1 --code 123456 --name fake` and `aetherdesk fake`.
- **See and drive the client window yourself:** build with `--features test-hooks` (never in release builds) and start it with `AETHERDESK_TEST_SOCKET=/tmp/ad.sock`. Then `scripts/hook.py /tmp/ad.sock state "key Right_Ctrl" "snapshot /tmp/w.png"` presses keys, clicks, and saves what the window shows (overlay included) as PNG you can look at. With `SDL_VIDEO_DRIVER=offscreen` it needs no display, so it works in the devcontainer and CI. In the user's own Hyprland session you can also screenshot the real window with `grim -g "<x>,<y> <w>x<h>"` (position from `hyprctl clients -j`); it shows the Mac's screen, so keep such images local.
- Run the client against the Mac: pair once (`aetherdesk pair <mac-ip>`, with the code from `scripts/mac.zsh pair` on the Mac), then `cargo run --release -p aetherdesk-linux-client` (or `-- <name>` with several paired Macs). The client keeps its identity and paired hosts in `~/.config/aetherdesk/`; in the devcontainer that's the container's home, so a rebuilt container has to pair again.
- The checkout is shared with the user, who may be testing a branch in it. For unrelated work, use a separate `git worktree` instead of switching branches.
