//! Where frames go between arriving and the screen (issue #109), and which
//! picture goes to the screen next.
//!
//! [`Frames`] counts every frame from the network to `present`: received,
//! decoded, dropped because the window was behind, replaced by a newer one
//! before it was shown, shown. Every [`REPORT_EVERY_US`] it gives one summary
//! line with the gaps between frames at capture (the host's timestamps), on
//! arrival and on screen. A gap on screen longer than [`SLOW_US`] also gets a
//! line of its own, with the frame's stage timings and a verdict:
//!
//! - *missing from the Mac*: the gap is in the capture timestamps already,
//!   so the host never sent the frame in between (capture or encoder);
//! - *replaced here*: a frame in between arrived but was replaced by a newer
//!   one before it was shown;
//! - *late*: no frame missing, this one just took longer (see its stages).
//!
//! [`Pacer`] picks the picture to show: the newest (the default, lowest
//! latency), or, with `AUROVAL_PRESENT=vsync`, one per refresh in order.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::latency::{Timing, stages};

/// A gap on screen longer than this (1.5 frames at 60 fps) is a visible skip.
pub const SLOW_US: i64 = 25_000;
/// Longer gaps mean the picture stood still: left out of the statistics.
pub const IDLE_US: i64 = 250_000;
/// How often the summary line comes.
pub const REPORT_EVERY_US: i64 = 5_000_000;
/// At most this many slow-frame lines per summary period; the rest are only
/// counted.
const SLOW_LINES: u32 = 5;

/// What the decode thread and the window report into.
pub struct Frames {
    inner: Mutex<Window>,
    /// One refresh of our monitor, µs.
    refresh_us: AtomicI64,
}

impl Default for Frames {
    /// For a 60 Hz monitor, until [`Frames::set_refresh_us`] says otherwise.
    fn default() -> Self {
        Self::new(16_667)
    }
}

#[derive(Default)]
struct Window {
    since_us: i64,
    received: u64,
    decoded: u64,
    /// Decoded pictures the window had no room for.
    queue_full: u64,
    /// Pictures replaced by a newer one before they were shown.
    replaced: u64,
    shown: u64,
    /// Shown less than half a refresh after the one before: the compositor
    /// probably never put it on screen.
    close: u64,
    last_pts: Option<i64>,
    last_received: Option<i64>,
    /// (presented, pts) of the last frame shown.
    last_shown: Option<(i64, i64)>,
    /// Dropped or replaced since the last frame shown.
    lost_since_shown: u64,
    capture_gaps: Vec<i64>,
    arrival_gaps: Vec<i64>,
    shown_gaps: Vec<i64>,
    late: u64,
    missing: u64,
    replaced_here: u64,
    slow_lines: u32,
}

/// Why a frame was shown late; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Missing,
    Replaced,
    Late,
}

impl Verdict {
    fn says(self) -> &'static str {
        match self {
            Verdict::Missing => "missing from the Mac",
            Verdict::Replaced => "replaced here",
            Verdict::Late => "late",
        }
    }
}

fn gap(v: &mut Vec<i64>, from: Option<i64>, to: i64) -> Option<i64> {
    let g = to - from?;
    (0..IDLE_US).contains(&g).then(|| {
        v.push(g);
        g
    })
}

/// Median, 95th percentile and maximum of `v`, in ms.
fn spread(v: &mut [i64]) -> String {
    if v.is_empty() {
        return "?".into();
    }
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
    format!("{:.1}/{:.1}/{:.1}", at(0.5), at(0.95), at(1.0))
}

impl Frames {
    /// For a monitor refreshing every `refresh_us`.
    pub fn new(refresh_us: i64) -> Self {
        Self {
            inner: Mutex::new(Window::default()),
            refresh_us: AtomicI64::new(refresh_us.max(1)),
        }
    }

    /// Our monitor refreshes every `us` microseconds.
    pub fn set_refresh_us(&self, us: i64) {
        self.refresh_us.store(us.max(1), Ordering::Relaxed);
    }

    /// A frame came off the network: captured at `pts` (host µs since the
    /// stream started), arrived at `received` (our µs).
    pub fn received(&self, pts: i64, received: i64) {
        let mut w = self.inner.lock().unwrap();
        w.received += 1;
        if w.since_us == 0 {
            w.since_us = received;
        }
        // A new stream (after a resize) starts its timestamps again.
        let last = w.last_pts.replace(pts);
        gap(&mut w.capture_gaps, last, pts);
        let last = w.last_received.replace(received);
        gap(&mut w.arrival_gaps, last, received);
    }

    pub fn decoded(&self) {
        self.inner.lock().unwrap().decoded += 1;
    }

    /// A decoded picture didn't fit in the window's queue.
    pub fn queue_full(&self) {
        let mut w = self.inner.lock().unwrap();
        w.queue_full += 1;
        w.lost_since_shown += 1;
    }

    /// `n` pictures were replaced before they were shown.
    pub fn replaced(&self, n: u64) {
        if n == 0 {
            return;
        }
        let mut w = self.inner.lock().unwrap();
        w.replaced += n;
        w.lost_since_shown += n;
    }

    /// A picture went to the screen. Returns the lines to print: about this
    /// frame when it came after a long gap, and the summary when due.
    pub fn shown(&self, t: &Timing) -> Vec<String> {
        let mut lines = Vec::new();
        let mut w = self.inner.lock().unwrap();
        w.shown += 1;
        let lost = std::mem::take(&mut w.lost_since_shown);
        if let Some((presented, pts)) = w.last_shown.replace((t.presented, t.pts)) {
            let on_screen = t.presented - presented;
            if (0..IDLE_US).contains(&on_screen) {
                w.shown_gaps.push(on_screen);
                if on_screen * 2 < self.refresh_us.load(Ordering::Relaxed) {
                    w.close += 1;
                }
                if on_screen > SLOW_US {
                    let in_capture = t.pts - pts;
                    let verdict = if lost > 0 {
                        Verdict::Replaced
                    } else if in_capture > SLOW_US {
                        Verdict::Missing
                    } else {
                        Verdict::Late
                    };
                    match verdict {
                        Verdict::Missing => w.missing += 1,
                        Verdict::Replaced => w.replaced_here += 1,
                        Verdict::Late => w.late += 1,
                    }
                    if w.slow_lines < SLOW_LINES {
                        w.slow_lines += 1;
                        lines.push(format!(
                            "  slow frame, {}: {:.1} ms since the last one shown, {:.1} ms \
                             apart at capture, {lost} replaced here | {}",
                            verdict.says(),
                            on_screen as f64 / 1000.0,
                            in_capture as f64 / 1000.0,
                            stages(t),
                        ));
                    }
                }
            }
        }
        if w.since_us != 0 && t.presented - w.since_us >= REPORT_EVERY_US {
            lines.push(w.summary(t.presented));
            *w = Window {
                since_us: t.presented,
                last_pts: w.last_pts,
                last_received: w.last_received,
                last_shown: w.last_shown,
                ..Window::default()
            };
        }
        lines
    }
}

impl Window {
    fn summary(&mut self, now: i64) -> String {
        let secs = (now - self.since_us) as f64 / 1e6;
        format!(
            "  frames in {secs:.1} s: {} received, {} decoded, {} dropped (window behind), \
             {} replaced before shown, {} shown ({} under half a refresh after the one \
             before) | gap ms median/p95/max: capture {}, arrival {}, shown {} | {} shown \
             gaps > 25 ms: {} missing from the Mac, {} replaced here, {} late",
            self.received,
            self.decoded,
            self.queue_full,
            self.replaced,
            self.shown,
            self.close,
            spread(&mut self.capture_gaps),
            spread(&mut self.arrival_gaps),
            spread(&mut self.shown_gaps),
            self.missing + self.replaced_here + self.late,
            self.missing,
            self.replaced_here,
            self.late,
        )
    }
}

/// How pictures go to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Present {
    /// Draw each picture as it arrives, newest first: the lowest latency.
    /// Without vsync two pictures can land in one refresh, and the
    /// compositor shows only the second.
    Latest,
    /// vsync on, one picture per refresh in arrival order: a picture that
    /// arrives just after a refresh waits for the next instead of being
    /// replaced. Up to one refresh more latency.
    Vsync,
}

/// The environment variable that picks [`Present`].
pub const PRESENT_ENV: &str = "AUROVAL_PRESENT";

impl Present {
    pub fn from_env() -> Self {
        match std::env::var(PRESENT_ENV).as_deref() {
            Ok("vsync") => Present::Vsync,
            _ => Present::Latest,
        }
    }
}

/// With [`Present::Vsync`]: pictures waiting beyond this are dropped, oldest
/// first, so latency can't build up.
const MAX_WAITING: usize = 2;
/// With [`Present::Vsync`]: when a picture has been left waiting after this
/// many refreshes in a row, the stream runs a little ahead of our monitor;
/// drop one to get the latency back down.
const TRIM_AFTER: u32 = 60;

/// Pictures between the decoder and the screen.
pub struct Pacer<T> {
    mode: Present,
    queue: VecDeque<T>,
    /// Refreshes in a row that left a picture waiting.
    backlog: u32,
}

impl<T> Pacer<T> {
    pub fn new(mode: Present) -> Self {
        Self {
            mode,
            queue: VecDeque::new(),
            backlog: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn push(&mut self, picture: T) {
        self.queue.push_back(picture);
    }

    /// The picture to draw now, if any, and how many were dropped unseen.
    pub fn next(&mut self) -> (Option<T>, u64) {
        let keep = match self.mode {
            Present::Latest => 1,
            Present::Vsync => MAX_WAITING,
        };
        let mut dropped = 0;
        while self.queue.len() > keep {
            self.queue.pop_front();
            dropped += 1;
        }
        let picture = self.queue.pop_front();
        if self.queue.is_empty() {
            self.backlog = 0;
        } else {
            self.backlog += 1;
            if self.backlog >= TRIM_AFTER {
                self.queue.pop_front();
                dropped += 1;
                self.backlog = 0;
            }
        }
        (picture, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: i64 = 16_667;

    fn timing(pts: i64, presented: i64) -> Timing {
        Timing {
            pts,
            presented,
            ..Timing::default()
        }
    }

    #[test]
    fn slow_frames_get_a_verdict() {
        let f = Frames::new(FRAME);
        let mut out = Vec::new();
        let mut show = |pts: i64, at: i64| out.extend(f.shown(&timing(pts, at)));
        f.received(0, 1_000);
        show(0, 1_000);
        // A frame missing at capture: 33 ms apart on the Mac already.
        f.received(2 * FRAME, 1_000 + 2 * FRAME);
        show(2 * FRAME, 1_000 + 2 * FRAME);
        // One arrives but is replaced before it's shown.
        f.received(3 * FRAME, 1_000 + 3 * FRAME);
        f.replaced(1);
        f.received(4 * FRAME, 1_000 + 4 * FRAME);
        show(4 * FRAME, 1_000 + 4 * FRAME);
        // The next frame on time at capture, but shown late.
        f.received(5 * FRAME, 1_000 + 6 * FRAME);
        show(5 * FRAME, 1_000 + 6 * FRAME);
        // Two shown within a few ms.
        f.received(6 * FRAME, 1_000 + 6 * FRAME + 3_000);
        show(6 * FRAME, 1_000 + 6 * FRAME + 3_000);
        assert_eq!(out.len(), 3, "{out:#?}");
        assert!(
            out[0].starts_with("  slow frame, missing from the Mac: 33.3 ms"),
            "{}",
            out[0]
        );
        assert!(
            out[0].contains("33.3 ms apart at capture, 0 replaced"),
            "{}",
            out[0]
        );
        assert!(
            out[1].starts_with("  slow frame, replaced here: 33.3 ms"),
            "{}",
            out[1]
        );
        assert!(out[1].contains("1 replaced here"), "{}", out[1]);
        assert!(
            out[2].starts_with("  slow frame, late: 33.3 ms"),
            "{}",
            out[2]
        );
        assert!(out[2].contains("16.7 ms apart at capture"), "{}", out[2]);

        // The summary, 5 s after the first frame.
        f.received(7 * FRAME, 5_100_000);
        let lines = f.shown(&timing(7 * FRAME, 5_100_000));
        let summary = lines.last().unwrap();
        assert!(summary.contains("frames in 5.1 s: 7 received"), "{summary}");
        assert!(
            summary.contains("1 replaced before shown, 6 shown (1 under half"),
            "{summary}"
        );
        assert!(
            summary
                .contains("3 shown gaps > 25 ms: 1 missing from the Mac, 1 replaced here, 1 late"),
            "{summary}"
        );
        // Capture gaps: one of 33.3 ms, five of 16.7.
        assert!(summary.contains("capture 16.7/33.3/33.3"), "{summary}");
    }

    #[test]
    fn a_new_period_starts_empty_and_limits_lines() {
        let f = Frames::new(FRAME);
        let mut t = 0;
        let mut lines = 0;
        for i in 0..20 {
            f.received(i * 2 * FRAME, t);
            lines += f.shown(&timing(i * 2 * FRAME, t)).len();
            t += 2 * FRAME;
        }
        assert_eq!(lines, SLOW_LINES as usize);
        t += 5_000_000;
        let out = f.shown(&timing(1_000 * FRAME, t));
        assert!(
            out.last()
                .unwrap()
                .contains("19 shown gaps > 25 ms: 19 missing")
        );
        let out = f.shown(&timing(1_001 * FRAME, t + FRAME));
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn latest_shows_the_newest() {
        let mut p = Pacer::new(Present::Latest);
        assert_eq!(p.next(), (None, 0));
        p.push(1);
        p.push(2);
        p.push(3);
        assert_eq!(p.next(), (Some(3), 2));
        assert!(p.is_empty());
    }

    #[test]
    fn vsync_shows_in_order_and_caps_the_wait() {
        let mut p = Pacer::new(Present::Vsync);
        // Two in one refresh: both shown, one per refresh.
        p.push(1);
        p.push(2);
        assert_eq!(p.next(), (Some(1), 0));
        assert_eq!(p.next(), (Some(2), 0));
        // A burst of four: the oldest two go.
        for i in 3..=6 {
            p.push(i);
        }
        assert_eq!(p.next(), (Some(5), 2));
        assert_eq!(p.next(), (Some(6), 0));
        assert_eq!(p.next(), (None, 0));
    }

    #[test]
    fn vsync_trims_a_standing_backlog() {
        let mut p = Pacer::new(Present::Vsync);
        p.push(0);
        // One arrives per refresh, but one is always waiting.
        let mut dropped = 0;
        for i in 1..=TRIM_AFTER as i32 {
            p.push(i);
            dropped += p.next().1;
        }
        assert_eq!(dropped, 1);
        assert!(p.is_empty());
    }
}
