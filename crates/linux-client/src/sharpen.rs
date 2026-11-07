//! `AUROVAL_SHARPEN=S` (0 < S ≤ 1): contrast-adaptive sharpening (after
//! AMD's FidelityFX CAS) of each decoded picture, on the CPU, before it's
//! drawn (#113). An experiment: it can put halos on text, so it's off
//! unless asked for. 4:2:0 pictures are sharpened in luma only, BGRA ones
//! per colour channel.

use crate::decoder::{Layout, Picture};

pub struct Sharpen {
    /// −1 / lerp(8, 5, S): the negative lobe at full contrast headroom.
    peak: f32,
    /// The plane before sharpening; neighbours are read from it.
    scratch: Vec<u8>,
}

impl Sharpen {
    pub fn from_env() -> Option<Self> {
        let v = std::env::var("AUROVAL_SHARPEN").ok()?;
        match parse(&v) {
            Some(s) => {
                say!("sharpening decoded pictures (AUROVAL_SHARPEN={s})");
                Some(Self::new(s))
            }
            None => {
                warn!("ignoring AUROVAL_SHARPEN={v}: wants a number above 0, up to 1");
                None
            }
        }
    }

    pub fn new(strength: f32) -> Self {
        let s = strength.clamp(0.0, 1.0);
        Self {
            peak: -1.0 / (8.0 + (5.0 - 8.0) * s),
            scratch: Vec::new(),
        }
    }

    pub fn apply(&mut self, p: &mut Picture) {
        let (w, h) = (p.width as usize, p.height as usize);
        let stride = p.strides[0];
        let (bpp, channels): (usize, &[usize]) = match p.layout {
            Layout::I420 | Layout::Nv12 => (1, &[0]),
            Layout::Bgra => (4, &[0, 1, 2]),
        };
        let plane = &mut p.planes[0];
        if w < 3 || h < 3 || plane.len() < stride * h {
            return;
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(&plane[..stride * h]);
        let src = &self.scratch[..];
        let peak = self.peak;
        // Rows in bands, one per core; the first and last row stay as they are.
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8);
        let band = (h - 2).div_ceil(threads);
        let rows = &mut plane[stride..stride * (h - 1)];
        std::thread::scope(|scope| {
            for (i, chunk) in rows.chunks_mut(band * stride).enumerate() {
                let first = 1 + i * band;
                scope.spawn(move || {
                    for (r, row) in chunk.chunks_mut(stride).enumerate() {
                        let y = first + r;
                        let rows = [
                            &src[(y - 1) * stride..y * stride],
                            &src[y * stride..(y + 1) * stride],
                            &src[(y + 1) * stride..(y + 2) * stride],
                        ];
                        sharpen_row(rows, row, w, bpp, channels, peak);
                    }
                });
            }
        });
    }
}

fn parse(v: &str) -> Option<f32> {
    v.trim()
        .parse::<f32>()
        .ok()
        .filter(|s| *s > 0.0 && *s <= 1.0)
}

/// One row from the `rows` above, at and below it in the source into
/// `out`, for the given channels, leaving the first and last pixel alone.
fn sharpen_row(
    rows: [&[u8]; 3],
    out: &mut [u8],
    w: usize,
    bpp: usize,
    channels: &[usize],
    peak: f32,
) {
    let [up, mid, down] = rows;
    let f = |v: u8| v as f32 * (1.0 / 255.0);
    for x in 1..w - 1 {
        let i = x * bpp;
        for &c in channels {
            let m = f(mid[i + c]);
            let n = f(up[i + c]);
            let s = f(down[i + c]);
            let e = f(mid[i + bpp + c]);
            let wst = f(mid[i - bpp + c]);
            let lo = m.min(n).min(s).min(e).min(wst);
            let hi = m.max(n).max(s).max(e).max(wst);
            // Less sharpening where the neighbourhood is already near black
            // or white, so edges don't ring past the range.
            let amp = if hi > 0.0 {
                (lo.min(1.0 - hi) / hi).clamp(0.0, 1.0).sqrt()
            } else {
                0.0
            };
            let wgt = amp * peak;
            let v = (m + wgt * (n + s + e + wst)) / (1.0 + 4.0 * wgt);
            out[i + c] = (v * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::Timing;

    fn grey(w: usize, h: usize, px: impl Fn(usize, usize) -> u8) -> Picture {
        let stride = w + 3; // wider than the row, as decoders do
        let mut y = vec![0u8; stride * h];
        for r in 0..h {
            for c in 0..w {
                y[r * stride + c] = px(c, r);
            }
        }
        Picture {
            width: w as u32,
            height: h as u32,
            layout: Layout::I420,
            planes: [y, vec![128; stride * h / 4], vec![128; stride * h / 4]],
            strides: [stride, stride / 2, stride / 2],
            timing: Timing::default(),
        }
    }

    #[test]
    fn strength_parses() {
        assert_eq!(parse("0.5"), Some(0.5));
        assert_eq!(parse("1"), Some(1.0));
        assert_eq!(parse("0"), None);
        assert_eq!(parse("2"), None);
        assert_eq!(parse("sharp"), None);
    }

    #[test]
    fn flat_pictures_stay_flat() {
        let mut p = grey(16, 9, |_, _| 120);
        let before = p.planes[0].clone();
        Sharpen::new(1.0).apply(&mut p);
        assert_eq!(p.planes[0], before);
    }

    #[test]
    fn edges_get_steeper() {
        // A ramp: 40, 55, 70, ... across.
        let mut p = grey(12, 12, |x, _| 40 + 15 * x as u8);
        let stride = p.strides[0];
        let before = p.planes[0].clone();
        // A ramp has no curvature: CAS leaves it (nearly) alone.
        Sharpen::new(1.0).apply(&mut p);
        for x in 1..11 {
            let (a, b) = (before[5 * stride + x], p.planes[0][5 * stride + x]);
            assert!(a.abs_diff(b) <= 1, "x {x}: {a} → {b}");
        }
        // A blurred step does: the dark side darker, the bright side brighter.
        let mut p = grey(12, 12, |x, _| match x {
            0..=4 => 80,
            5 => 110,
            6 => 150,
            _ => 180,
        });
        let before = p.planes[0].clone();
        Sharpen::new(1.0).apply(&mut p);
        let row = &p.planes[0][5 * stride..];
        assert!(row[5] < 110, "dark side {}", row[5]);
        assert!(row[6] > 150, "bright side {}", row[6]);
        // Far from the edge nothing changes; the borders are left alone.
        assert_eq!(row[2], 80);
        assert_eq!(row[10], 180);
        assert_eq!(p.planes[0][..12], before[..12]);
    }

    #[test]
    fn bgra_leaves_alpha() {
        let (w, h) = (8usize, 8usize);
        let mut px = vec![0u8; w * h * 4];
        for (i, chunk) in px.chunks_mut(4).enumerate() {
            let v = if i % w < 4 { 60 } else { 200 };
            chunk.copy_from_slice(&[v, v, v, 255]);
        }
        let mut p = Picture {
            width: w as u32,
            height: h as u32,
            layout: Layout::Bgra,
            planes: [px, Vec::new(), Vec::new()],
            strides: [w * 4, 0, 0],
            timing: Timing::default(),
        };
        Sharpen::new(0.5).apply(&mut p);
        assert!(p.planes[0].chunks(4).all(|c| c[3] == 255));
    }
}
