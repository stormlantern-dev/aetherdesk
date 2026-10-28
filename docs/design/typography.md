# Typography and key hints

Part of the UI rules from #34. Used by the client overlay (#16, #31 P1) and,
later, the HUD, host picker and pairing dialog. Colours in the mockups are
provisional; the palette is a later step of #34.

## Fonts

| Role | Font | File | Licence |
|---|---|---|---|
| UI (title, body, hint, key caps) | Inter 4.1 Regular, SemiBold | `assets/fonts/Inter-*.ttf` | OFL 1.1 |
| Numbers (HUD) | JetBrains Mono 2.304 Regular | `assets/fonts/JetBrainsMono-Regular.ttf` | OFL 1.1 |

Bundled into the binary (`include_bytes!`), so nothing depends on system
fonts. About 1.1 MB in total. Why this pair: Inter is built for UI text at
small sizes and has a wide glyph set (arrows and key symbols included);
JetBrains Mono has clear, equal-width digits for numbers that change every
frame. IBM Plex and Atkinson Hyperlegible were compared too, see
[font-comparison.png](font-comparison.png).

## Type scale

Sizes are **logical points**. The client multiplies them by the window's
display scale (`SDL_GetWindowDisplayScale`) and renders text at that pixel
size, so text is sharp and the same physical size at Hyprland scale 1, 1.25,
1.5 or 2.

| Token | Size | Weight | Use |
|---|---|---|---|
| `title` | 20 pt | SemiBold | What is going on: one line per screen |
| `body` | 15 pt | Regular | Why, and the action next to a key cap |
| `hint` | 13 pt | Regular | Details (error reason, address); the key-cap label (SemiBold) |
| `mono` | 13 pt | Regular, JetBrains Mono | Numbers in the HUD |

- **Minimum: 12 pt.** No text is ever smaller, whatever the window size.
- Text follows the **display scale, not the picture.** The overlay is drawn in
  window pixels with the renderer's logical size switched off (as `window.rs`
  already does). The 3456×2234 Mac picture shrinks to fit the window; overlay
  text doesn't shrink with it. That is what keeps 13 pt readable on a
  3456-px picture in Fit on a 2560×1440 monitor.
- Line height: the font's own (ascent + descent). Gap between lines in a block: 6 pt.
- **Text never sits directly on the video.** It goes on a surface (≥ 90 %
  opaque) or on the dimmed picture inside a card. A busy Mac desktop behind
  bare text is unreadable at any size.

## Key hints

A key hint is a **key cap followed by the action**: `[Enter] Reconnect now`.

- **Key cap:** rounded box (radius 5 pt, 1 pt border) around the key name in
  `hint` SemiBold, 7 pt horizontal padding, at least as wide as it is tall.
  Gap between cap and action: 8 pt. Gap between two hints: 24 pt.
- **Key names are words, as printed on the keyboard:** `Enter`, `Esc`,
  `Right Ctrl`. No symbols like ⏎ or ⎋: not everyone reads them, and the
  word needs no explanation.
- **The host key is shown by its configured name** (#24), never as a
  literal "Right Ctrl" in the code. The mockups use the default.
- **One primary action per screen.** Its key cap is filled (light cap, dark
  label) and its action is in full `text` colour. Secondary actions have an
  outlined cap and a muted action. Primary goes first (left).
- Actions are verbs, short, sentence case: `Reconnect now`, `Retry`, `Quit`,
  `Release`, `Send input to the Mac`.
- Text and key cap are vertically centred on the cap height, so they line up
  on one optical middle.

## Layout

- **Cards** (reconnecting, disconnected, can't reach): centred, over the
  picture dimmed to ~33 % brightness. Order: title, body, detail (`hint`,
  muted), 20 pt gap, key hints. Padding 24 pt, radius 12 pt.
- **Badges** (LOCAL, → Mac, VIEW ONLY): one-line pill at the bottom left of
  the *picture* (not the window), inset 16 pt, clear of the Mac's menu bar
  (top) and Dock (bottom middle). A coloured tag with a word comes first:
  status is never colour alone.
- Badges stay one line. If one doesn't fit, drop the detail before shrinking text.

## Copy

Starting copy, from `crates/linux-client/src/window.rs`, reworked for the
title / body / hint hierarchy:

| Screen | Title / tag | Body | Detail | Hints |
|---|---|---|---|---|
| Reconnecting | Connection lost | Reconnecting to *host* (attempt *n*)… | *reason* | **Enter** Reconnect now · Esc Quit |
| Disconnected (gave up) | Disconnected from *host* | No connection for a minute, so it stopped trying. | *reason* | **Enter** Retry · Esc Quit |
| Can't reach (before first frame) | Can’t reach *host* | Is the Mac awake and on the same network? | *reason* | **Enter** Retry · Esc Quit |
| Input local | `LOCAL` | Input stays here | or click the picture | *host key* Send input to the Mac |
| Input to Mac (toast) | `→ Mac` | Keyboard and mouse go to the Mac | | *host key* Release |
| Login window | `VIEW ONLY` | The Mac is at the login window | Log in at the Mac, or with Screen Sharing, to take control | |

The reconnecting screen now shows Enter: the client already retries on Enter
while reconnecting, but the old text only said "Esc to quit".

## Mockups

In [`mockups/`](mockups/), each with display scale 1.0 (top) and 1.5
(bottom), cropped from a 2560×1440 window at actual pixels (view at 100 %):
`reconnecting`, `failed`, `unreachable`, `local`, `toast`, `login`.
`overview-local.png` shows the whole window (half size) for placement.
Regenerate with `python3 docs/design/render_mockups.py` (Pillow with libraqm).
