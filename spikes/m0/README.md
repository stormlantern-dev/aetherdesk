# M0 spike — capture and input at the macOS lock screen / login window

Throwaway tool for the go/no-go spikes in [docs/MVP-PLAN.md](../../docs/MVP-PLAN.md) (M0). Results go on issue #3.

`m0 probe` takes a ScreenCaptureKit screenshot every second (PNG), logs session / permission / Secure Event Input state, and after a delay types `aetherdesk` via `CGEventPost` — **never followed by Return**, so it can't cause a failed login attempt.

Everything below runs **on the Mac**, in Terminal (zsh). `mac/m0.zsh` wraps the steps.

> Why launchd jobs and not just `./m0` in Terminal? A process started from Terminal borrows *Terminal's* Screen Recording / Accessibility permissions. To test `m0`'s own permissions — as the real agent will need them — it has to run as its own launchd job.

## One-time setup

1. **Rust:** install via [rustup](https://rustup.rs) and clone this repo.
2. **Signing certificate** (free, no Apple account needed): Keychain Access → *Keychain Access* menu → *Certificate Assistant* → *Create a Certificate…*
   - Name: `AetherDesk Dev`
   - Identity Type: *Self Signed Root*
   - Certificate Type: *Code Signing*
3. **Build, sign, install:**
   ```zsh
   spikes/m0/mac/m0.zsh install
   ```
   Installs `/Users/Shared/aetherdesk/m0` and writes the launchd plists. The first `codesign` asks for keychain access — choose *Always Allow*.
4. **Permissions:** System Settings → Privacy & Security → add `/Users/Shared/aetherdesk/m0` (with `+`, then Cmd+Shift+G to type the path) to both **Screen & System Audio Recording** and **Accessibility**.

## Tests

Each test leaves a folder in `/Users/Shared/aetherdesk-m0/` with `log.txt` and `frame-NNNN.png`. `m0.zsh latest` shows the newest log and opens the folder. After each test, **clear the password field** before typing your real password.

| Command | What it answers |
|---|---|
| `m0.zsh lock-hid` | User-session agent, posts to the HID tap: do frames show the lock screen, and do dots appear? |
| `m0.zsh lock-session` | Same, posting to the session tap. |
| `m0.zsh lock-root` | Root LaunchDaemon (asks for your password via `sudo`): can a daemon outside the GUI session capture and/or type? |
| `m0.zsh loginwindow-on`, then log out | LoginWindow agent: does it start, capture the login window, type into it? Run `loginwindow-off` afterwards. |
| `m0.zsh install` again | Rebuild + re-sign: are the permissions still granted? (`screen-capture-access=true` in the next log) |

Things to look for in `log.txt`:

- `frame NNNN FAILED: …` — the capture error, e.g. no permission or no display at the lock screen.
- `secure-input=true` — Secure Event Input is on (expected while the password field is focused).
- `session changed: …` — the `CGSSessionScreenIsLocked` key flips when you lock.

Also note if macOS asks something like "continue to allow screen recording?" for `m0` in the days after.

`m0.zsh cleanup` removes all launchd jobs.
