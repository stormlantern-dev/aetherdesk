"""Render the overlay mockups in docs/design/mockups/ (issue #34).

    python3 docs/design/render_mockups.py

Needs Pillow with libraqm. Uses only the fonts in assets/fonts/. The Mac
picture behind the overlay is fake: a busy 3456x2234 desktop, shown in Fit
on a 2560x1440 window. All sizes are logical points; `scale` is the display
scale (1.0, 1.5). Rules: docs/design/typography.md.
"""
import random
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FONTS = ROOT / "assets" / "fonts"

from PIL import Image, ImageDraw, ImageFilter, ImageFont

# --- type scale (logical pt) -------------------------------------------------
TITLE, BODY, HINT, MONO, MIN_PT = 20, 15, 13, 13, 12
# --- provisional colours (palette is a later #34 step) -----------------------
DIM = (0, 0, 0, 170)
SURFACE = (22, 22, 26, 235)
TEXT = (242, 242, 245)
MUTED = (170, 170, 182)
CAP_LINE = (120, 120, 134)
CAP_FILL = (44, 44, 52)
PRIMARY_FILL = (242, 242, 245)
PRIMARY_TEXT = (18, 18, 22)
AMBER = (255, 200, 80)
BLUE = (110, 170, 255)
HOST_KEY = "Right Ctrl"  # from config (#24); never hard-coded in the client

SS = 4  # supersampling for shapes


# --- the fake Mac picture ----------------------------------------------------
def mac_picture(w=3456, h=2234, seed=7):
    """A busy 3456x2234 'Mac screen': wallpaper, windows, text, code."""
    rnd = random.Random(seed)
    img = Image.new("RGB", (w // 8, h // 8))
    d = ImageDraw.Draw(img)
    for _ in range(40):
        x, y = rnd.randrange(img.width), rnd.randrange(img.height)
        r = rnd.randrange(30, 160)
        c = rnd.choice([(255, 120, 60), (60, 140, 255), (200, 60, 200), (40, 200, 160), (250, 220, 90), (30, 40, 90)])
        d.ellipse((x - r, y - r, x + r, y + r), fill=c)
    img = img.filter(ImageFilter.GaussianBlur(30)).resize((w, h), Image.BILINEAR)
    d = ImageDraw.Draw(img)
    ui = ImageFont.truetype(str(FONTS / "Inter-Regular.ttf"), 26)
    mono = ImageFont.truetype(str(FONTS / "JetBrainsMono-Regular.ttf"), 24)
    words = ("the quick brown fox jumps over lazy dog remote desktop stream frame "
             "latency encoder capture display window cursor keyboard mouse").split()
    # menu bar
    d.rectangle((0, 0, w, 48), fill=(236, 236, 238))
    for i, t in enumerate(["Finder", "File", "Edit", "View", "Go", "Window", "Help"]):
        d.text((90 + i * 140, 10), t, font=ui, fill=(20, 20, 20))
    wins = [(160, 180, 1500, 1300, "light"), (1200, 420, 2900, 1800, "code"),
            (2500, 200, 3350, 900, "light"), (300, 1350, 1700, 2100, "dark"),
            (2200, 1500, 3300, 2150, "light")]
    for x0, y0, x1, y1, kind in wins:
        bg = {"light": (250, 250, 250), "dark": (40, 40, 46), "code": (30, 32, 40)}[kind]
        d.rounded_rectangle((x0 + 12, y0 + 16, x1 + 12, y1 + 16), 22, fill=(0, 0, 0))
        d.rounded_rectangle((x0, y0, x1, y1), 22, fill=bg)
        d.rectangle((x0, y0 + 60, x1, y0 + 62), fill=(200, 200, 200) if kind == "light" else (70, 70, 80))
        for j, c in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
            d.ellipse((x0 + 24 + j * 40, y0 + 20, x0 + 48 + j * 40, y0 + 44), fill=c)
        y = y0 + 90
        while y < y1 - 40:
            if kind == "code":
                x = x0 + 40 + rnd.randrange(0, 4) * 48
                for _ in range(rnd.randrange(2, 7)):
                    t = rnd.choice(words) + rnd.choice(["(", ".", " = ", "::", ";", ""])
                    col = rnd.choice([(220, 220, 230), (255, 130, 160), (130, 200, 255), (180, 230, 130), (240, 200, 120)])
                    d.text((x, y), t, font=mono, fill=col)
                    x += mono.getlength(t + " ")
                    if x > x1 - 200:
                        break
                y += 36
            else:
                fg = (30, 30, 30) if kind == "light" else (220, 220, 225)
                t = " ".join(rnd.choice(words) for _ in range(rnd.randrange(4, 14)))
                d.text((x0 + 40, y), t[: int((x1 - x0 - 80) / 13)], font=ui, fill=fg)
                y += 40
    # dock
    d.rounded_rectangle((1100, 2080, 2356, 2210), 30, fill=(220, 220, 228))
    for i in range(14):
        c = rnd.choice([(255, 120, 60), (60, 140, 255), (200, 60, 200), (40, 200, 160), (250, 220, 90)])
        d.rounded_rectangle((1130 + i * 88, 2100, 1210 + i * 88, 2180), 18, fill=c)
    return img


def fit(picture, win_w, win_h):
    """Letterbox the picture into a window (Fit mode); returns image + picture rect."""
    s = min(win_w / picture.width, win_h / picture.height)
    pw, ph = round(picture.width * s), round(picture.height * s)
    out = Image.new("RGB", (win_w, win_h), (0, 0, 0))
    out.paste(picture.resize((pw, ph), Image.LANCZOS), ((win_w - pw) // 2, (win_h - ph) // 2))
    return out, ((win_w - pw) // 2, (win_h - ph) // 2, pw, ph)


# --- overlay drawing ---------------------------------------------------------
class Fonts:
    def __init__(self, fontdir, ui, mono):
        self.dir, self.ui, self.mono = Path(fontdir), ui, mono

    def get(self, role, scale):
        file, pt = {
            "title": (f"{self.ui}-SemiBold.ttf", TITLE),
            "body": (f"{self.ui}-Regular.ttf", BODY),
            "hint": (f"{self.ui}-Regular.ttf", HINT),
            "cap": (f"{self.ui}-SemiBold.ttf", HINT),
            "tag": (f"{self.ui}-SemiBold.ttf", HINT),
            "mono": (f"{self.mono}-Regular.ttf", MONO),
        }[role]
        return ImageFont.truetype(str(self.dir / file), round(max(pt, MIN_PT) * scale),
                                  layout_engine=ImageFont.Layout.RAQM)


class Painter:
    """Shapes are supersampled; text is drawn directly at device pixels."""

    def __init__(self, base, fonts, scale):
        self.base, self.f, self.s = base.convert("RGBA"), fonts, scale
        self.ops = []  # deferred (shape layer must be composited before text)

    def px(self, v):
        return round(v * self.s)

    def rect(self, box, radius, fill, outline=None, width=0):
        self.ops.append(("rect", box, radius, fill, outline, width))

    def text(self, xy, s, role, fill):
        self.ops.append(("text", xy, s, role, fill))

    def text_mid(self, x, y_mid, s, role, fill):
        """Text optically centred on y_mid: cap height straddles it."""
        cap_h = -self.f.get(role, self.s).getbbox("H", anchor="ls")[1]
        self.ops.append(("text_ls", (x, round(y_mid + cap_h / 2)), s, role, fill))

    def poly(self, pts, fill):
        self.ops.append(("poly", pts, fill))

    def width(self, s, role):
        return self.f.get(role, self.s).getlength(s)

    def line_h(self, role):
        f = self.f.get(role, self.s)
        a, d = f.getmetrics()
        return a + d

    def finish(self):
        # Composite each shape on its own so translucent fills stack like
        # SDL blending does; text goes on top in order.
        out = self.base
        for op in self.ops:
            if op[0] in ("rect", "poly"):
                layer = Image.new("RGBA", (out.width * SS, out.height * SS), (0, 0, 0, 0))
                d = ImageDraw.Draw(layer)
                if op[0] == "rect":
                    _, (x0, y0, x1, y1), r, fill, outline, w = op
                    d.rounded_rectangle((x0 * SS, y0 * SS, x1 * SS - 1, y1 * SS - 1), r * SS,
                                        fill=fill, outline=outline, width=w * SS)
                else:
                    d.polygon([(x * SS, y * SS) for x, y in op[1]], fill=op[2])
                out = Image.alpha_composite(out, layer.resize(out.size, Image.LANCZOS))
            else:
                kind, (x, y), s, role, fill = op
                ImageDraw.Draw(out).text((x, y), s, font=self.f.get(role, self.s), fill=fill,
                                         anchor="ls" if kind == "text_ls" else "la")
        return out.convert("RGB")


def key_cap(p, x, y_mid, key, primary=False):
    """Draw a key cap centred on y_mid; returns its width in device px."""
    pad = p.px(7)
    h = p.line_h("cap") + p.px(4)
    w = max(p.width(key, "cap") + 2 * pad, h)
    y0 = round(y_mid - h / 2)
    if primary:
        p.rect((x, y0, x + w, y0 + h), p.px(5), PRIMARY_FILL)
    else:
        p.rect((x, y0, x + w, y0 + h), p.px(5), CAP_FILL, CAP_LINE, max(1, p.px(1)))
    tx = x + (w - p.width(key, "cap")) / 2
    p.text_mid(tx, y_mid, key, "cap", PRIMARY_TEXT if primary else TEXT)
    return round(w)


def hint_row_width(p, hints):
    w = 0
    for i, (key, action, _) in enumerate(hints):
        cap_w = max(p.width(key, "cap") + 2 * p.px(7), p.line_h("cap") + p.px(4))
        w += cap_w + p.px(8) + p.width(action, "body") + (p.px(24) if i else 0)
    return w


def hint_row(p, x, y_mid, hints):
    """hints: [(key, action, primary)] -> draws `[key] action` pairs."""
    for i, (key, action, primary) in enumerate(hints):
        if i:
            x += p.px(24)
        x += key_cap(p, x, y_mid, key, primary) + p.px(8)
        p.text_mid(x, y_mid, action, "body", TEXT if primary else MUTED)
        x += p.width(action, "body")
    return x


def card(p, area, title, body, detail, hints):
    """Centred card: title / body / detail / hints."""
    ax, ay, aw, ah = area
    pad, gap = p.px(24), p.px(6)
    lines = [(title, "title", TEXT), (body, "body", TEXT)] + ([(detail, "hint", MUTED)] if detail else [])
    content_w = max([p.width(s, r) for s, r, _ in lines] + [hint_row_width(p, hints)])
    cap_h = p.line_h("cap") + p.px(4)
    content_h = sum(p.line_h(r) for _, r, _ in lines) + gap * (len(lines) - 1) + p.px(20) + cap_h
    w, h = content_w + 2 * pad, content_h + 2 * pad
    x0, y0 = round(ax + (aw - w) / 2), round(ay + (ah - h) / 2)
    p.rect((x0, y0, x0 + w, y0 + h), p.px(12), SURFACE)
    y = y0 + pad
    for s, r, c in lines:
        p.text((x0 + pad, y), s, r, c)
        y += p.line_h(r) + gap
    hint_row(p, x0 + pad, y - gap + p.px(20) + cap_h / 2, hints)


def tag(p, x, y_mid, label, colour):
    """Coloured status tag with a word: never colour alone."""
    pad = p.px(7)
    h = p.line_h("tag") + p.px(4)
    w = p.width(label, "tag") + 2 * pad
    y0 = round(y_mid - h / 2)
    p.rect((x, y0, x + w, y0 + h), p.px(5), colour)
    tx = x + pad
    p.text_mid(tx, y_mid, label, "tag", PRIMARY_TEXT)
    return round(w)


def badge(p, pic_rect, tag_label, colour, text, hints=(), detail=None):
    """Bottom-left pill inside the picture (clear of menu bar and Dock)."""
    px0, py0, pw, ph = pic_rect
    pad, inset = p.px(10), p.px(16)
    h = p.line_h("tag") + p.px(4) + 2 * p.px(8)
    tw = p.width(tag_label, "tag") + 2 * p.px(7)
    w = tw + p.px(10) + p.width(text, "body")
    if hints:
        w += p.px(20) + hint_row_width(p, hints)
    if detail:
        w += p.px(10) + p.width(detail, "hint")
    w += 2 * pad
    x0, y1 = px0 + inset, py0 + ph - inset
    y0 = y1 - h
    p.rect((x0, y0, x0 + w, y1), p.px(10), SURFACE)
    mid = y0 + h / 2
    x = x0 + pad + tag(p, x0 + pad, mid, tag_label, colour) + p.px(10)
    p.text_mid(x, mid, text, "body", TEXT)
    x += p.width(text, "body")
    if hints:
        x = hint_row(p, x + p.px(20), mid, hints)
    if detail:
        p.text_mid(x + p.px(10), mid, detail, "hint", MUTED)


# --- screens -----------------------------------------------------------------
HOST = "MacBook Pro"


def screen(name, base, pic_rect, fonts, scale):
    """Draw one #16 screen over `base` (already letterboxed)."""
    p = Painter(base, fonts, scale)
    W, H = base.size
    if name in ("reconnecting", "failed"):
        p.rect((0, 0, W, H), 0, DIM)
        if name == "reconnecting":
            card(p, (0, 0, W, H), "Connection lost",
                 f"Reconnecting to {HOST} (attempt 2)…",
                 "No answer from the Mac for 3 seconds",
                 [("Enter", "Reconnect now", True), ("Esc", "Quit", False)])
        else:
            card(p, (0, 0, W, H), f"Disconnected from {HOST}",
                 "No connection for a minute, so it stopped trying.",
                 "connection timed out",
                 [("Enter", "Retry", True), ("Esc", "Quit", False)])
    elif name == "unreachable":
        # Before the first frame there is no picture: plain dark window.
        p.rect((0, 0, W, H), 0, (16, 16, 19, 255))
        card(p, (0, 0, W, H), f"Can’t reach {HOST}",
             "Is the Mac awake and on the same network?",
             "no answer from 192.0.2.10:4500",
             [("Enter", "Retry", True), ("Esc", "Quit", False)])
    elif name == "local":
        badge(p, pic_rect, "LOCAL", AMBER, "Input stays here",
              [(HOST_KEY, "Send input to the Mac", False)], detail="or click the picture")
    elif name == "toast":
        badge(p, pic_rect, "→ Mac", BLUE, "Keyboard and mouse go to the Mac",
              [(HOST_KEY, "Release", False)])
    elif name == "login":
        badge(p, pic_rect, "VIEW ONLY", AMBER, "The Mac is at the login window",
              detail="Log in at the Mac, or with Screen Sharing, to take control")
    return p.finish()


def mockups(fontdir, ui, mono, outdir):
    fonts = Fonts(fontdir, ui, mono)
    win, rect = fit(mac_picture(), 2560, 1440)
    # Fewer colours in the fake background keeps the PNGs small; the
    # overlay itself is drawn in full colour on top.
    win = win.quantize(64, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).convert("RGB")
    outdir = Path(outdir)
    outdir.mkdir(parents=True, exist_ok=True)
    label_f = ImageFont.truetype(str(Path(fontdir) / f"{ui}-Regular.ttf"), 22)
    centre = (640, 440, 1920, 1000)    # cards
    corner = (166, 1180, 1446, 1440)   # bottom-left badges (picture starts at x=166)
    for name in ["reconnecting", "failed", "unreachable", "local", "toast", "login"]:
        box = corner if name in ("local", "toast", "login") else centre
        bw, bh = box[2] - box[0], box[3] - box[1]
        sheet = Image.new("RGB", (bw, 2 * (bh + 40)), (12, 12, 14))
        d = ImageDraw.Draw(sheet)
        for i, scale in enumerate([1.0, 1.5]):
            img = screen(name, win, rect, fonts, scale)
            if name == "local" and scale == 1.0:
                # Whole window, half size: shows where badges sit.
                img.resize((1280, 720), Image.LANCZOS).save(outdir / "overview-local.png", optimize=True)
            d.text((12, i * (bh + 40) + 8), f"Display scale {scale:g}: actual pixels of a 2560×1440 window",
                   font=label_f, fill=(200, 200, 205))
            sheet.paste(img.crop(box), (0, i * (bh + 40) + 40))
        sheet.save(outdir / f"{name}.png", optimize=True)

if __name__ == "__main__":
    mockups(FONTS, "Inter", "JetBrainsMono", ROOT / "docs" / "design" / "mockups")
