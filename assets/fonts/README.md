# Bundled fonts

Embedded in the Linux client so it never depends on system fonts. Usage
rules: [docs/design/typography.md](../../docs/design/typography.md).

| File | Font | Version | Source | Licence |
|---|---|---|---|---|
| `Inter-Regular.ttf`, `Inter-SemiBold.ttf` | Inter | 4.1 | <https://github.com/rsms/inter/releases/tag/v4.1> (`extras/ttf`) | SIL OFL 1.1, [`Inter-OFL.txt`](Inter-OFL.txt) |
| `JetBrainsMono-Regular.ttf` | JetBrains Mono | 2.304 | <https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304> (`fonts/ttf`) | SIL OFL 1.1, [`JetBrainsMono-OFL.txt`](JetBrainsMono-OFL.txt) |

The files are unmodified. The OFL allows bundling them with the software;
the licence files must ship alongside (also in packages, #37).
