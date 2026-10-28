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

Shell is zsh. The host builds natively (a container can't use ScreenCaptureKit or VideoToolbox). Don't run `sudo` yourself; give the user the command.

1. **Get the PR and check the machine:**
   ```zsh
   scripts/mac.zsh pr <number>
   ```
   This switches to the PR's branch and runs `aether-agent doctor` (permissions, display size, IP addresses). If a permission is MISSING, tell the user exactly what to grant — System Settings → Privacy & Security → *Screen & System Audio Recording* and/or *Accessibility*, for the terminal app the agent runs from — and that the terminal must be restarted afterwards. You can't click those yourself.

2. **Run the host** in the background so the user can connect from Linux:
   ```zsh
   scripts/mac.zsh serve
   ```
   With `AETHERDESK_TOKEN` set in the environment, the host uses that token instead of a random one, so the Linux side needs nothing from you. Otherwise tell the user the token it printed.

3. **Tell the user what to try**, based on the PR description and the issue's test plan, and wait for their observations. Ask short questions; don't make them write essays.

4. **Report on the PR** with `gh pr comment <number>`: what was tested on the Mac, what worked, what didn't, and relevant host output (`serve` log lines, `doctor` output, errors). **Never include the token** — the repo is public. Mention which issue test-plan items the results cover, but leave ticking the boxes to the verification on the issue.

Other host commands: `scripts/mac.zsh record --seconds 10` (capture + encode to `/tmp/aetherdesk.h264`, no network), `scripts/mac.zsh update [branch]`.

### Installed host (M2: daemon + agents)

From M2 on, the host also runs **installed**: a root daemon (`aether-agent daemon`, the network side) plus an agent per GUI session (`aether-agent agent`, capture and input), started by launchd. That's what makes the locked and logged-out cases work.

- `scripts/mac.zsh install` builds, signs with the self-signed identity (`AETHERDESK_IDENTITY`, default "AetherDesk Dev") and installs. It needs `sudo`, so the **user** runs it, not you. It keeps the token in a root-only file, `/Library/Application Support/AetherDesk/token`: set from `AETHERDESK_TOKEN` when that is set, otherwise random.
- After the first install, the user grants **Screen Recording** and **Accessibility** to `/Library/Application Support/AetherDesk/aether-agent` (not to the terminal), once.
- `scripts/mac.zsh status`: whether the daemon and agent run, plus the last log lines. `scripts/mac.zsh logs` follows the logs in `/Library/Logs/AetherDesk/`. Paste relevant lines into your report (never the token).
- While installed, the daemon holds UDP 4500, so `serve` can't bind. Use `serve --port 4501`, or `scripts/mac.zsh uninstall` first.
- After testing a PR that changes the host, re-run `scripts/mac.zsh install` so the installed copy matches the branch.

## On Linux

Shell is **nushell** — give the user commands in nushell syntax. Never run `sudo`; give the user the command instead.

Rust runs in the devcontainer (Podman):

```nu
devcontainer up --docker-path podman --workspace-folder .
devcontainer exec --docker-path podman --workspace-folder . bash -lc 'cargo test --workspace'
```

- Type-check the Mac host from Linux: `cargo clippy -p aetherdesk-mac-host --all-targets --target aarch64-apple-darwin` (works in the devcontainer image; nothing is linked).
- Test the client without a Mac: `scripts/e2e-fake-host.sh` (CI runs it too), or `cargo run --example fake_host` and connect to `127.0.0.1` with token `test`.
- Run the client against the Mac: `cargo run --release -p aetherdesk-linux-client -- <mac-ip> --token <token>`, or set `AETHERDESK_HOST` and `AETHERDESK_TOKEN` and drop the arguments.
- The checkout is shared with the user, who may be testing a branch in it. For unrelated work, use a separate `git worktree` instead of switching branches.

## Until M3 pairing exists

Set the same secret on both machines once, so testing needs no copying:

- Mac (zsh, `~/.zshrc`): `export AETHERDESK_TOKEN=<secret>`
- Linux (nushell, `env.nu`): `$env.AETHERDESK_TOKEN = "<secret>"` and `$env.AETHERDESK_HOST = "<mac-ip>"`
