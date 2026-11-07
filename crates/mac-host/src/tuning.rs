//! Picture-quality switches (#113), read once from the environment. One is
//! on by default: we capture at twice the picture size and scale down
//! ourselves, which keeps small text crisper than ScreenCaptureKit's own
//! scaling (measured on the Mac: crisper glyphs for about +4 ms and a few
//! percent CPU). The rest are experiments, off unless set.
//!
//! - `AUROVAL_BPP=F`: at least F bits per pixel per frame, on top of
//!   `--mbps` (the bitrate is whichever is higher, at most [`MAX_BITRATE`]).
//! - `AUROVAL_QUALITY_FIRST=1`: don't ask the encoder to put speed over
//!   quality (`PrioritizeEncodingSpeedOverQuality` off).
//! - `AUROVAL_MAX_QP=N`: the encoder may quantise no coarser than N
//!   (`MaxAllowedFrameQP`; 1–51, lower is sharper and bigger).
//! - `AUROVAL_REFINE=N`: once the picture has stood still for
//!   [`REFINE_AFTER`], encode the last frame again, up to N times, so the
//!   encoder can add the detail a starved frame (e.g. the keyframe after a
//!   resize) left out.
//! - `AUROVAL_CAPTURE_SCALE=1|2|3|full`: capture at the picture size and
//!   let ScreenCaptureKit scale (`1` or `off`), or at 2× (default) or 3× the
//!   picture size, at most the display's, or at the display's size, and scale
//!   down ourselves (VideoToolbox pixel transfer).
//! - `AUROVAL_DOWNSAMPLE=average|decimate`: how that scaling filters
//!   (default average: a box filter).
//! - `AUROVAL_DUMP_FRAME=PREFIX`: save the first frame of each capture
//!   size, as it goes into the encoder, as `PREFIX-captured-WxH.png`.

use std::time::Duration;

/// Highest bitrate [`bitrate_bps`] asks for.
pub const MAX_BITRATE: u32 = 400_000_000;

/// How long the picture must stand still before [`Tuning::refine`] kicks in,
/// and the gap between refinements.
pub const REFINE_AFTER: Duration = Duration::from_millis(100);

/// What to capture at, relative to the picture that's encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureScale {
    /// At the picture size; ScreenCaptureKit scales.
    Picture,
    /// At this many times the picture size, capped at the display's.
    Times(u32),
    /// At the display's size.
    Display,
}

impl Default for CaptureScale {
    /// Twice the picture size: ScreenCaptureKit's own scaling blurs small
    /// text, a box filter from 2× keeps it crisp (#113).
    fn default() -> Self {
        CaptureScale::Times(2)
    }
}

/// How our own downscale filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Downsample {
    /// Averages the source pixels each output pixel covers.
    #[default]
    Average,
    /// Picks source pixels; for comparison.
    Decimate,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tuning {
    pub bpp: Option<f64>,
    pub quality_first: bool,
    pub max_qp: Option<u32>,
    pub refine: u32,
    pub capture_scale: CaptureScale,
    pub downsample: Downsample,
    pub dump: Option<String>,
}

impl Tuning {
    /// The switches as `var` reports them; unparsable values are ignored
    /// (and reported in the second value).
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut bad = Vec::new();
        let parse = |name: &str| -> Option<String> {
            var(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut t = Tuning::default();
        if let Some(v) = parse("AUROVAL_BPP") {
            match v.parse::<f64>() {
                Ok(f) if f > 0.0 && f.is_finite() => t.bpp = Some(f),
                _ => bad.push(format!("AUROVAL_BPP={v}")),
            }
        }
        if let Some(v) = parse("AUROVAL_QUALITY_FIRST") {
            t.quality_first = v != "0";
        }
        if let Some(v) = parse("AUROVAL_MAX_QP") {
            match v.parse::<u32>() {
                Ok(n @ 1..=51) => t.max_qp = Some(n),
                _ => bad.push(format!("AUROVAL_MAX_QP={v}")),
            }
        }
        if let Some(v) = parse("AUROVAL_REFINE") {
            match v.parse::<u32>() {
                Ok(n) => t.refine = n.min(30),
                _ => bad.push(format!("AUROVAL_REFINE={v}")),
            }
        }
        if let Some(v) = parse("AUROVAL_CAPTURE_SCALE") {
            match v.to_ascii_lowercase().as_str() {
                "full" | "display" => t.capture_scale = CaptureScale::Display,
                "1" | "off" => t.capture_scale = CaptureScale::Picture,
                n => match n.parse::<u32>() {
                    Ok(n @ 2..=8) => t.capture_scale = CaptureScale::Times(n),
                    _ => bad.push(format!("AUROVAL_CAPTURE_SCALE={v}")),
                },
            }
        }
        if let Some(v) = parse("AUROVAL_DOWNSAMPLE") {
            match v.to_ascii_lowercase().as_str() {
                "average" | "box" => t.downsample = Downsample::Average,
                "decimate" => t.downsample = Downsample::Decimate,
                _ => bad.push(format!("AUROVAL_DOWNSAMPLE={v}")),
            }
        }
        t.dump = parse("AUROVAL_DUMP_FRAME");
        (t, bad)
    }

    /// The process's switches, read from the environment the first time.
    pub fn get() -> &'static Tuning {
        static TUNING: std::sync::OnceLock<Tuning> = std::sync::OnceLock::new();
        TUNING.get_or_init(|| {
            let (t, bad) = Tuning::from_vars(|name| std::env::var(name).ok());
            for b in bad {
                eprintln!("tuning: ignoring {b}");
            }
            if t != Tuning::default() {
                println!("tuning (#113): {t:?}");
            }
            t
        })
    }
}

/// The average bitrate for a `size` picture at `fps`: `mbps`, or more when
/// `bpp` asks for more bits per pixel per frame.
pub fn bitrate_bps(mbps: u32, size: (usize, usize), fps: u32, bpp: Option<f64>) -> u32 {
    let base = mbps.saturating_mul(1_000_000);
    let Some(bpp) = bpp else {
        return base;
    };
    let floor = bpp * size.0 as f64 * size.1 as f64 * fps.max(1) as f64;
    let floor = floor.min(MAX_BITRATE as f64) as u32;
    base.max(floor)
}

/// What to capture `display` at, for a `picture`-sized encode: `picture`
/// itself, or bigger for our own downscale. Even sizes, never bigger than
/// the display; a multiple that doesn't fit takes the display's size.
pub fn capture_size(display: (u32, u32), picture: (u32, u32), scale: CaptureScale) -> (u32, u32) {
    let display = (display.0 & !1, display.1 & !1);
    match scale {
        CaptureScale::Picture => picture,
        CaptureScale::Display => display,
        CaptureScale::Times(n) => {
            let (w, h) = (picture.0.saturating_mul(n), picture.1.saturating_mul(n));
            if w > display.0 || h > display.1 {
                display
            } else {
                (w, h)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> (Tuning, Vec<String>) {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Tuning::from_vars(|k| map.get(k).cloned())
    }

    #[test]
    fn nothing_set_is_the_default() {
        assert_eq!(vars(&[]), (Tuning::default(), vec![]));
        assert_eq!(Tuning::default().capture_scale, CaptureScale::Times(2));
        assert_eq!(
            vars(&[("AUROVAL_CAPTURE_SCALE", "off")]).0.capture_scale,
            CaptureScale::Picture
        );
        // Empty counts as unset.
        assert_eq!(vars(&[("AUROVAL_BPP", "")]).0, Tuning::default());
    }

    #[test]
    fn switches_parse() {
        let (t, bad) = vars(&[
            ("AUROVAL_BPP", "0.8"),
            ("AUROVAL_QUALITY_FIRST", "1"),
            ("AUROVAL_MAX_QP", "30"),
            ("AUROVAL_REFINE", "3"),
            ("AUROVAL_CAPTURE_SCALE", "2"),
            ("AUROVAL_DOWNSAMPLE", "decimate"),
            ("AUROVAL_DUMP_FRAME", "/tmp/f"),
        ]);
        assert!(bad.is_empty());
        assert_eq!(t.bpp, Some(0.8));
        assert!(t.quality_first);
        assert_eq!(t.max_qp, Some(30));
        assert_eq!(t.refine, 3);
        assert_eq!(t.capture_scale, CaptureScale::Times(2));
        assert_eq!(t.downsample, Downsample::Decimate);
        assert_eq!(t.dump.as_deref(), Some("/tmp/f"));
        assert_eq!(
            vars(&[("AUROVAL_CAPTURE_SCALE", "Full")]).0.capture_scale,
            CaptureScale::Display
        );
        assert!(!vars(&[("AUROVAL_QUALITY_FIRST", "0")]).0.quality_first);
    }

    #[test]
    fn bad_values_are_ignored() {
        let (t, bad) = vars(&[
            ("AUROVAL_BPP", "-1"),
            ("AUROVAL_MAX_QP", "99"),
            ("AUROVAL_CAPTURE_SCALE", "1.5"),
            ("AUROVAL_DOWNSAMPLE", "lanczos"),
        ]);
        assert_eq!(t, Tuning::default());
        assert_eq!(bad.len(), 4);
    }

    #[test]
    fn bitrate_has_a_per_pixel_floor() {
        // Without a floor: just --mbps.
        assert_eq!(bitrate_bps(20, (1040, 674), 60, None), 20_000_000);
        // 20 Mbit/s is already ~0.48 bit per pixel at 1040x674, 60 fps:
        // a floor below that changes nothing...
        assert_eq!(bitrate_bps(20, (1040, 674), 60, Some(0.4)), 20_000_000);
        // ...one above it raises the rate.
        assert_eq!(bitrate_bps(20, (1040, 674), 60, Some(1.0)), 42_057_600);
        // Capped, also for silly values.
        assert_eq!(bitrate_bps(20, (3456, 2234), 60, Some(10.0)), MAX_BITRATE);
        assert_eq!(bitrate_bps(u32::MAX, (8, 8), 60, None), u32::MAX);
    }

    #[test]
    fn capture_is_a_multiple_capped_at_the_display() {
        let mac = (3456, 2234);
        let pic = (1040, 672);
        assert_eq!(capture_size(mac, pic, CaptureScale::Picture), pic);
        assert_eq!(capture_size(mac, pic, CaptureScale::Times(2)), (2080, 1344));
        assert_eq!(capture_size(mac, pic, CaptureScale::Times(3)), (3120, 2016));
        // 4× doesn't fit: the display, not something in between.
        assert_eq!(capture_size(mac, pic, CaptureScale::Times(4)), mac);
        assert_eq!(capture_size(mac, pic, CaptureScale::Display), mac);
        // A full-size picture stays full size.
        assert_eq!(capture_size(mac, mac, CaptureScale::Times(2)), mac);
        // Odd display sizes come out even.
        assert_eq!(
            capture_size((1921, 1081), (640, 360), CaptureScale::Display),
            (1920, 1080)
        );
    }
}
