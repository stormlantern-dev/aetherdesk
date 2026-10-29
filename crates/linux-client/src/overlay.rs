//! What the window shows over the picture, after `docs/design/typography.md`
//! (#34): cards for a lost connection, badges for where input goes, key
//! hints as a key cap plus an action. Text is Inter, built into the binary
//! and drawn with ab_glyph, at sizes in points times the display scale, so
//! it's sharp and the same physical size whatever the window or picture.
//!
//! Each card or badge is drawn once into an RGBA image and shown as a
//! texture until its content or the scale changes.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

use crate::net::State;

static INTER: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
static INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");

fn regular() -> &'static FontRef<'static> {
    static F: OnceLock<FontRef<'static>> = OnceLock::new();
    F.get_or_init(|| FontRef::try_from_slice(INTER).expect("bundled font"))
}

fn semibold() -> &'static FontRef<'static> {
    static F: OnceLock<FontRef<'static>> = OnceLock::new();
    F.get_or_init(|| FontRef::try_from_slice(INTER_SEMIBOLD).expect("bundled font"))
}

// The type scale, in points.
const TITLE: f32 = 20.0;
const BODY: f32 = 15.0;
const HINT: f32 = 13.0;
const LINE_GAP: f32 = 6.0;

// Colours (provisional until the palette step of #34).
type Rgba = [u8; 4];
const SURFACE: Rgba = [24, 24, 28, 240];
const TEXT: Rgba = [238, 238, 242, 255];
const MUTED: Rgba = [160, 160, 172, 255];
const CAP_LINE: Rgba = [150, 150, 162, 255];
const AMBER: Rgba = [255, 196, 64, 255];
const BLUE: Rgba = [96, 164, 255, 255];
const ON_TAG: Rgba = [20, 20, 24, 255];

/// A key cap followed by what the key does: `[Enter] Reconnect now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: String,
    pub action: String,
    /// The one main action: filled cap, full-colour action.
    pub primary: bool,
}

fn hint(key: &str, action: &str, primary: bool) -> Hint {
    Hint {
        key: key.into(),
        action: action.into(),
        primary,
    }
}

/// Centred over the dimmed picture: the connection is lost or gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub title: String,
    pub body: String,
    pub detail: String,
    pub hints: Vec<Hint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Amber,
    Blue,
}

/// A one-line pill at the picture's bottom left: where input goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// A word on a coloured tag: status is never colour alone.
    pub tag: String,
    pub tone: Tone,
    pub body: String,
    pub detail: String,
    pub hints: Vec<Hint>,
}

/// Everything over the picture. Compared between frames to know when to
/// draw it again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overlay {
    /// Darken the picture: it's not live.
    pub dim: bool,
    pub card: Option<Card>,
    /// Bottom-left badges, the bottom one last.
    pub badges: Vec<Badge>,
}

impl Overlay {
    pub fn new(
        state: &State,
        host: &str,
        host_key: &str,
        grabbed: bool,
        toast: bool,
        stalled: bool,
        at_login: bool,
    ) -> Self {
        let mut o = Overlay::default();
        match state {
            State::Connected => {
                if stalled {
                    o.badges.push(Badge {
                        tag: "STALLED".into(),
                        tone: Tone::Amber,
                        body: "No new picture from the Mac".into(),
                        detail: String::new(),
                        hints: Vec::new(),
                    });
                }
                if at_login {
                    o.badges.push(Badge {
                        tag: "VIEW ONLY".into(),
                        tone: Tone::Amber,
                        body: "The Mac is at the login window".into(),
                        detail: "Log in at the Mac, or with Screen Sharing, to take control".into(),
                        hints: Vec::new(),
                    });
                }
                if !grabbed {
                    o.badges.push(Badge {
                        tag: "LOCAL".into(),
                        tone: Tone::Amber,
                        body: "Input stays here".into(),
                        detail: "or click the picture".into(),
                        hints: vec![hint(host_key, "Send input to the Mac", false)],
                    });
                } else if toast {
                    o.badges.push(Badge {
                        tag: "→ Mac".into(),
                        tone: Tone::Blue,
                        body: "Keyboard and mouse go to the Mac".into(),
                        detail: String::new(),
                        hints: vec![hint(host_key, "Release", false)],
                    });
                }
            }
            State::Reconnecting { attempt, reason } => {
                o.dim = true;
                o.card = Some(Card {
                    title: "Connection lost".into(),
                    body: format!("Reconnecting to {host} (attempt {attempt})…"),
                    detail: short(reason),
                    hints: vec![
                        hint("Enter", "Reconnect now", true),
                        hint("Esc", "Quit", false),
                    ],
                });
            }
            State::GaveUp { reason } => {
                o.dim = true;
                o.card = Some(Card {
                    title: format!("Disconnected from {host}"),
                    body: "No connection for a minute, so it stopped trying.".into(),
                    detail: short(reason),
                    hints: vec![hint("Enter", "Retry", true), hint("Esc", "Quit", false)],
                });
            }
        }
        o
    }

    pub fn is_empty(&self) -> bool {
        !self.dim && self.card.is_none() && self.badges.is_empty()
    }
}

/// Long reasons get cut; the full text is in the terminal and the log.
fn short(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// An RGBA image, straight alpha, row by row.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    fn new(width: f32, height: f32) -> Self {
        let (w, h) = (width.ceil().max(1.0) as u32, height.ceil().max(1.0) as u32);
        Self {
            width: w,
            height: h,
            rgba: vec![0; (w * h * 4) as usize],
        }
    }

    /// Blends `c` at `cover` (0..1) over the pixel at (x, y).
    fn blend(&mut self, x: i64, y: i64, c: Rgba, cover: f32) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 || cover <= 0.0 {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        let a = (c[3] as f32 / 255.0) * cover.min(1.0);
        let da = self.rgba[i + 3] as f32 / 255.0;
        let out = a + da * (1.0 - a);
        if out <= 0.0 {
            return;
        }
        for (k, &src) in c.iter().take(3).enumerate() {
            let dst = self.rgba[i + k] as f32;
            self.rgba[i + k] = ((src as f32 * a + dst * da * (1.0 - a)) / out).round() as u8;
        }
        self.rgba[i + 3] = (out * 255.0).round() as u8;
    }

    /// A filled rounded rectangle, anti-aliased.
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let r = r.min(w / 2.0).min(h / 2.0);
        for py in y.floor() as i64..(y + h).ceil() as i64 {
            for px in x.floor() as i64..(x + w).ceil() as i64 {
                // Signed distance from the pixel centre to the shape.
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                let qx = (cx - (x + w / 2.0)).abs() - (w / 2.0 - r);
                let qy = (cy - (y + h / 2.0)).abs() - (h / 2.0 - r);
                let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
                let d = outside + qx.max(qy).min(0.0) - r;
                self.blend(px, py, c, 0.5 - d);
            }
        }
    }

    /// `text` with its baseline at `y`, starting at `x`.
    fn text(&mut self, x: f32, y: f32, text: &str, font: &FontRef<'static>, px: f32, c: Rgba) {
        let scaled = font.as_scaled(PxScale::from(px));
        let mut caret = x;
        let mut last = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(prev) = last {
                caret += scaled.kern(prev, id);
            }
            let glyph = id.with_scale_and_position(px, point(caret, y));
            caret += scaled.h_advance(id);
            last = Some(id);
            if let Some(outlined) = font.outline_glyph(glyph) {
                let b = outlined.px_bounds();
                outlined.draw(|gx, gy, cover| {
                    self.blend(
                        b.min.x as i64 + gx as i64,
                        b.min.y as i64 + gy as i64,
                        c,
                        cover,
                    );
                });
            }
        }
    }
}

/// Width of `text` in pixels.
fn width(text: &str, font: &FontRef<'static>, px: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut last = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = last {
            w += scaled.kern(prev, id);
        }
        w += scaled.h_advance(id);
        last = Some(id);
    }
    w
}

/// Ascent and descent (positive) of a font at `px`.
fn metrics(font: &FontRef<'static>, px: f32) -> (f32, f32) {
    let scaled = font.as_scaled(PxScale::from(px));
    (scaled.ascent(), -scaled.descent())
}

/// Lays out a row of key hints at scale `s` (pixels per point): its width
/// and height, and a function that draws it with its top left at (x, y).
struct HintRow<'a> {
    hints: &'a [Hint],
    s: f32,
}

impl HintRow<'_> {
    fn cap(&self, h: &Hint) -> (f32, f32) {
        let label = width(&h.key, semibold(), HINT * self.s);
        let (a, d) = metrics(semibold(), HINT * self.s);
        let height = a + d + 6.0 * self.s;
        ((label + 14.0 * self.s).max(height), height)
    }

    fn size(&self) -> (f32, f32) {
        let mut w = 0.0;
        let mut h: f32 = 0.0;
        for (i, hint) in self.hints.iter().enumerate() {
            let (cw, ch) = self.cap(hint);
            let (a, d) = metrics(regular(), BODY * self.s);
            w += cw + 8.0 * self.s + width(&hint.action, regular(), BODY * self.s);
            if i > 0 {
                w += 24.0 * self.s;
            }
            h = h.max(ch).max(a + d);
        }
        (w, h)
    }

    fn draw(&self, img: &mut Image, x: f32, y: f32) {
        let (_, row_h) = self.size();
        let mut cx = x;
        for (i, hint) in self.hints.iter().enumerate() {
            if i > 0 {
                cx += 24.0 * self.s;
            }
            let (cw, ch) = self.cap(hint);
            let top = y + (row_h - ch) / 2.0;
            let r = 5.0 * self.s;
            let label = if hint.primary {
                img.round_rect(cx, top, cw, ch, r, TEXT);
                SURFACE_SOLID
            } else {
                // Outline: the line colour, then the surface inside it.
                img.round_rect(cx, top, cw, ch, r, CAP_LINE);
                let t = self.s.max(1.0);
                img.round_rect(
                    cx + t,
                    top + t,
                    cw - 2.0 * t,
                    ch - 2.0 * t,
                    r - t,
                    SURFACE_SOLID,
                );
                TEXT
            };
            let (la, ld) = metrics(semibold(), HINT * self.s);
            let lw = width(&hint.key, semibold(), HINT * self.s);
            // Centre the label on the cap; text and cap share an optical middle.
            let baseline = top + (ch - (la + ld)) / 2.0 + la;
            img.text(
                cx + (cw - lw) / 2.0,
                baseline,
                &hint.key,
                semibold(),
                HINT * self.s,
                label,
            );
            cx += cw + 8.0 * self.s;
            let (ba, bd) = metrics(regular(), BODY * self.s);
            let baseline = y + (row_h - (ba + bd)) / 2.0 + ba;
            let colour = if hint.primary { TEXT } else { MUTED };
            img.text(cx, baseline, &hint.action, regular(), BODY * self.s, colour);
            cx += width(&hint.action, regular(), BODY * self.s);
        }
    }
}

/// The surface colour, opaque: for drawing inside an already drawn surface.
const SURFACE_SOLID: Rgba = [SURFACE[0], SURFACE[1], SURFACE[2], 255];

/// A card as an image, at `s` pixels per point, no wider than `max_w`.
pub fn render_card(card: &Card, s: f32, max_w: f32) -> Image {
    let pad = 24.0 * s;
    let lines = [
        (&card.title, semibold(), TITLE, TEXT),
        (&card.body, regular(), BODY, TEXT),
        (&card.detail, regular(), HINT, MUTED),
    ];
    let row = HintRow {
        hints: &card.hints,
        s,
    };
    let (row_w, row_h) = row.size();
    let mut w = row_w;
    let mut h = 0.0;
    for (i, (text, font, pt, _)) in lines.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let (a, d) = metrics(font, pt * s);
        w = w.max(width(text, font, pt * s));
        h += a + d + if i > 0 { LINE_GAP * s } else { 0.0 };
    }
    if !card.hints.is_empty() {
        h += 20.0 * s + row_h;
    }
    let (cw, ch) = ((w + 2.0 * pad).min(max_w), h + 2.0 * pad);
    let mut img = Image::new(cw, ch);
    img.round_rect(0.0, 0.0, cw, ch, 12.0 * s, SURFACE);
    let mut y = pad;
    for (i, (text, font, pt, colour)) in lines.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        if i > 0 {
            y += LINE_GAP * s;
        }
        let (a, d) = metrics(font, pt * s);
        img.text(pad, y + a, text, font, pt * s, *colour);
        y += a + d;
    }
    if !card.hints.is_empty() {
        row.draw(&mut img, pad, y + 20.0 * s);
    }
    img
}

/// A badge as an image, at `s` pixels per point, no wider than `max_w`: the
/// detail goes first, before anything gets cut.
pub fn render_badge(badge: &Badge, s: f32, max_w: f32) -> Image {
    let full = badge_image(badge, s);
    if full.width as f32 <= max_w || badge.detail.is_empty() {
        return full;
    }
    let shorter = Badge {
        detail: String::new(),
        ..badge.clone()
    };
    badge_image(&shorter, s)
}

fn badge_image(badge: &Badge, s: f32) -> Image {
    let pad = 10.0 * s;
    let gap = 10.0 * s;
    let (ta, td) = metrics(semibold(), HINT * s);
    let tag_w = width(&badge.tag, semibold(), HINT * s) + 12.0 * s;
    let tag_h = ta + td + 6.0 * s;
    let (ba, bd) = metrics(regular(), BODY * s);
    let row = HintRow {
        hints: &badge.hints,
        s,
    };
    let (row_w, row_h) = row.size();
    let inner_h = tag_h.max(ba + bd).max(row_h);
    let h = inner_h + 2.0 * 7.0 * s;
    let mut w = pad + tag_w + gap + width(&badge.body, regular(), BODY * s);
    if !badge.detail.is_empty() {
        w += gap + width(&badge.detail, regular(), HINT * s);
    }
    if !badge.hints.is_empty() {
        w += 16.0 * s + row_w;
    }
    w += pad + 4.0 * s;
    let mut img = Image::new(w, h);
    img.round_rect(0.0, 0.0, w, h, h / 2.0, SURFACE);
    let mid = h / 2.0;
    let mut x = pad;
    let tone = match badge.tone {
        Tone::Amber => AMBER,
        Tone::Blue => BLUE,
    };
    img.round_rect(x, mid - tag_h / 2.0, tag_w, tag_h, 5.0 * s, tone);
    img.text(
        x + 6.0 * s,
        mid - (ta + td) / 2.0 + ta,
        &badge.tag,
        semibold(),
        HINT * s,
        ON_TAG,
    );
    x += tag_w + gap;
    img.text(
        x,
        mid - (ba + bd) / 2.0 + ba,
        &badge.body,
        regular(),
        BODY * s,
        TEXT,
    );
    x += width(&badge.body, regular(), BODY * s);
    if !badge.detail.is_empty() {
        x += gap;
        let (ha, hd) = metrics(regular(), HINT * s);
        img.text(
            x,
            mid - (ha + hd) / 2.0 + ha,
            &badge.detail,
            regular(),
            HINT * s,
            MUTED,
        );
        x += width(&badge.detail, regular(), HINT * s);
    }
    if !badge.hints.is_empty() {
        row.draw(&mut img, x + 16.0 * s, mid - row_h / 2.0);
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_give_the_designed_copy() {
        let o = Overlay::new(
            &State::Connected,
            "Mac",
            "Right Ctrl",
            true,
            false,
            false,
            false,
        );
        assert!(o.is_empty());
        let local = Overlay::new(
            &State::Connected,
            "Mac",
            "ScrollLock",
            false,
            false,
            false,
            false,
        );
        assert_eq!(local.badges[0].tag, "LOCAL");
        assert_eq!(local.badges[0].hints[0].key, "ScrollLock");
        let toast = Overlay::new(
            &State::Connected,
            "Mac",
            "Right Ctrl",
            true,
            true,
            false,
            false,
        );
        assert_eq!(toast.badges[0].tag, "→ Mac");
        let lost = Overlay::new(
            &State::Reconnecting {
                attempt: 2,
                reason: "no answer".into(),
            },
            "Office Mac",
            "Right Ctrl",
            false,
            false,
            false,
            false,
        );
        assert!(lost.dim);
        let card = lost.card.unwrap();
        assert!(card.body.contains("Office Mac") && card.body.contains("attempt 2"));
        assert_eq!(
            (card.hints[0].key.as_str(), card.hints[0].primary),
            ("Enter", true)
        );
        let gone = Overlay::new(
            &State::GaveUp { reason: "x".into() },
            "Mac",
            "Right Ctrl",
            false,
            false,
            false,
            false,
        );
        assert_eq!(gone.card.unwrap().hints[0].action, "Retry");
        let login = Overlay::new(
            &State::Connected,
            "Mac",
            "Right Ctrl",
            true,
            false,
            false,
            true,
        );
        assert_eq!(login.badges[0].tag, "VIEW ONLY");
    }

    #[test]
    fn images_scale_with_the_display() {
        let o = Overlay::new(
            &State::Connected,
            "Mac",
            "Right Ctrl",
            false,
            false,
            false,
            false,
        );
        let one = render_badge(&o.badges[0], 1.0, 10_000.0);
        let two = render_badge(&o.badges[0], 2.0, 10_000.0);
        assert!((two.width as f32 / one.width as f32 - 2.0).abs() < 0.1);
        assert!(
            one.rgba.chunks(4).any(|p| p[3] == 255),
            "drawn opaque somewhere"
        );
        // Too narrow: the detail goes first.
        let narrow = render_badge(&o.badges[0], 1.0, one.width as f32 - 1.0);
        assert!(narrow.width < one.width);
    }

    #[test]
    fn long_reasons_are_cut() {
        assert_eq!(short(&"x".repeat(150)).chars().count(), 101);
        assert_eq!(short(" ok "), "ok");
    }
}
