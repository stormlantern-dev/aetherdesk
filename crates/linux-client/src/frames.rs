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
//! A second line, *pacing*, follows a frame through every point it passes
//! (captured, delivered, submitted, encoded, sent on the Mac; received,
//! decoded, shown here) and gives the gap to the frame before at each, plus
//! how many came under half a refresh after the one before: *bunched*, two
//! in one refresh. Where that count jumps, between two points, is the stage
//! that makes frames arrive in pairs; the line names it. The host's points
//! are on its own clock, so no clock sync is needed for gaps.
//!
//! [`Pacer`] picks the picture to show; see [`Present`] for the modes
//! (`AETHERDESK_PRESENT`).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use aetherdesk_proto::HostTiming;

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
    /// Gaps at each of [`POINTS`] but the last (shown: `shown_gaps`).
    points: [Track; POINTS.len() - 1],
    /// How long `present` took for a picture: with vsync, the wait for
    /// the compositor.
    present_us: Vec<i64>,
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

/// The points a frame passes, in order. The pacing line gives the gaps
/// between frames at each.
const POINTS: [&str; 8] = [
    "captured",
    "delivered",
    "submitted",
    "encoded",
    "sent",
    "received",
    "decoded",
    "shown",
];
/// What lies between `POINTS[i]` and `POINTS[i + 1]` (the stage names of
/// the latency line).
const BETWEEN: [&str; 7] = [
    "capture",
    "encoder wait",
    "encode",
    "host queue",
    "network",
    "decode queue + decode",
    "to screen",
];

/// The gaps between frames at one point.
#[derive(Default, Clone)]
struct Track {
    last: Option<i64>,
    gaps: Vec<i64>,
}

impl Track {
    /// A frame passed at `t`; zero is "not known".
    fn at(&mut self, t: i64) {
        if t != 0 {
            let last = self.last.replace(t);
            gap(&mut self.gaps, last, t);
        }
    }

    /// For a new period: the gaps go, the last time stays.
    fn fresh(&self) -> Self {
        Self {
            last: self.last,
            gaps: Vec::new(),
        }
    }
}

/// How many of `gaps` are under `us`.
fn under(gaps: &[i64], us: i64) -> usize {
    gaps.iter().filter(|&&g| g < us).count()
}

/// Median and 95th percentile of `v`, in ms.
fn mid_p95(v: &mut [i64]) -> String {
    if v.is_empty() {
        return "?".into();
    }
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
    format!("{:.1}/{:.1}", at(0.5), at(0.95))
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

    /// One refresh of our monitor, µs.
    pub fn refresh_us(&self) -> i64 {
        self.refresh_us.load(Ordering::Relaxed)
    }

    /// A frame came off the network: captured at `pts` (host µs since the
    /// stream started), arrived at `received` (our µs), with the host's
    /// stamps `host` (its clock).
    pub fn received(&self, host: &HostTiming, pts: i64, received: i64) {
        let mut w = self.inner.lock().unwrap();
        w.received += 1;
        let host = [
            host.captured_us,
            host.delivered_us,
            host.submitted_us,
            host.encoded_us,
            host.sent_us,
        ];
        for (track, t) in w.points.iter_mut().zip(host) {
            track.at(t as i64);
        }
        w.points[5].at(received);
        if w.since_us == 0 {
            w.since_us = received;
        }
        // A new stream (after a resize) starts its timestamps again.
        let last = w.last_pts.replace(pts);
        gap(&mut w.capture_gaps, last, pts);
        let last = w.last_received.replace(received);
        gap(&mut w.arrival_gaps, last, received);
    }

    /// A picture came out of the decoder at `at` (our µs).
    pub fn decoded(&self, at: i64) {
        let mut w = self.inner.lock().unwrap();
        w.decoded += 1;
        w.points[6].at(at);
    }

    /// `present` took `us` for a picture.
    pub fn present_took(&self, us: i64) {
        self.inner.lock().unwrap().present_us.push(us);
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
            lines.push(w.pacing(self.refresh_us.load(Ordering::Relaxed)));
            *w = Window {
                since_us: t.presented,
                last_pts: w.last_pts,
                last_received: w.last_received,
                last_shown: w.last_shown,
                points: w.points.clone().map(|p| p.fresh()),
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

    /// The pacing line: gaps at every point, how many bunched (under half
    /// a refresh), and where most new bunching starts.
    fn pacing(&mut self, refresh_us: i64) -> String {
        let close = refresh_us / 2;
        let mut parts = Vec::with_capacity(POINTS.len());
        let mut bunched = Vec::with_capacity(POINTS.len());
        for (i, name) in POINTS.iter().enumerate() {
            let gaps = match self.points.get_mut(i) {
                Some(track) => &mut track.gaps,
                None => &mut self.shown_gaps,
            };
            // A point without stamps (an older host) has no gaps at all.
            let n = (!gaps.is_empty()).then(|| under(gaps, close));
            bunched.push(n);
            parts.push(format!(
                "{name} {} [{}]",
                mid_p95(gaps),
                n.map_or("?".into(), |n| n.to_string())
            ));
        }
        let waited = self
            .present_us
            .iter()
            .filter(|&&us| us * WAITED_SHARE > refresh_us)
            .count();
        format!(
            "  pacing, gap to the frame before in ms median/p95 [bunched: under half a \
             refresh]: {} | {} | present() ms median/p95/max {}, {waited} waited",
            parts.join(", "),
            where_bunching_starts(&bunched),
            spread(&mut self.present_us),
        )
    }
}

/// Names the stage with the biggest rise in bunched frames from one point
/// to the next, from the counts at each point (`None`: no stamps there).
/// Fewer bunched at the screen than before it is normal: the newest of two
/// replaces the other.
fn where_bunching_starts(bunched: &[Option<usize>]) -> String {
    // (from point, to point, rise), between neighbours with stamps.
    let mut best: Option<(usize, usize, usize)> = None;
    let mut last: Option<(usize, usize)> = None;
    for (i, n) in bunched.iter().enumerate() {
        let Some(n) = *n else { continue };
        if let Some((j, before)) = last
            && n > before
            && best.is_none_or(|(_, _, rise)| n - before > rise)
        {
            best = Some((j, i, n - before));
        }
        last = Some((i, n));
    }
    match best {
        None => "no stage bunches frames".into(),
        Some((from, to, rise)) => format!(
            "most bunching starts in: {} (+{rise})",
            BETWEEN[from..to].join(" + ")
        ),
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
    /// replaced. Up to one refresh more latency, and once pictures queue up
    /// (frames arrive in pairs), they stay queued: measured +35 ms (#109).
    Vsync,
    /// vsync on, but nothing queues: the newest picture only, as with
    /// [`Present::Latest`]. On Wayland, SDL hands a picture to the
    /// compositor at once when the compositor has taken the one before
    /// (its frame callback came), and otherwise waits for that callback,
    /// which comes right after the next refresh. So a picture on its own
    /// goes out as fast as with `Latest`; the second of two that arrive in
    /// one refresh is shown one refresh later instead of replacing the
    /// first. What arrives during that wait replaces each other, newest
    /// wins. Latency: unchanged for most frames, at most one refresh more
    /// for the second of a pair. See [`Pacer::presented`] for the one case
    /// where waiting would become the rule.
    LatestVsync,
}

/// The environment variable that picks [`Present`].
pub const PRESENT_ENV: &str = "AETHERDESK_PRESENT";

impl Present {
    pub fn from_env() -> Self {
        Self::parse(std::env::var(PRESENT_ENV).ok().as_deref())
    }

    fn parse(v: Option<&str>) -> Self {
        match v.map(str::trim) {
            Some("vsync") => Present::Vsync,
            Some("latest-vsync") => Present::LatestVsync,
            _ => Present::Latest,
        }
    }

    /// Whether this mode presents with vsync on.
    pub fn vsync(self) -> bool {
        self != Present::Latest
    }

    /// SDL hints the mode needs; before SDL starts. [`Present::LatestVsync`]
    /// relies on how SDL's EGL path waits on Wayland (frame callback, then
    /// swap), so it asks for the OpenGL renderer; `SDL_RENDER_DRIVER` in the
    /// environment still wins.
    pub fn set_hints(self) {
        if self == Present::LatestVsync {
            sdl3::hint::set("SDL_RENDER_DRIVER", "opengl,opengles2");
        }
    }

    /// What the log says about it.
    pub fn describe(self) -> &'static str {
        match self {
            Present::Latest => "presenting as pictures come",
            Present::Vsync => "presenting with vsync, one picture per refresh",
            Present::LatestVsync => "presenting with vsync, newest picture only (latest-vsync)",
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
/// With [`Present::LatestVsync`]: `present` taking more than this fraction
/// (1/N) of a refresh waited for the compositor.
const WAITED_SHARE: i64 = 8;
/// With [`Present::LatestVsync`]: after this many waits in a row, leave one
/// picture out to get back ahead (see [`Pacer::presented`]).
const CATCH_UP_AFTER: u32 = 30;

/// Pictures between the decoder and the screen.
pub struct Pacer<T> {
    mode: Present,
    queue: VecDeque<T>,
    /// Refreshes in a row that left a picture waiting.
    backlog: u32,
    /// With [`Present::LatestVsync`]: presents in a row that waited.
    waited: u32,
    /// With [`Present::LatestVsync`]: leave the next picture out.
    skip: bool,
}

impl<T> Pacer<T> {
    pub fn new(mode: Present) -> Self {
        Self {
            mode,
            queue: VecDeque::new(),
            backlog: 0,
            waited: 0,
            skip: false,
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
        if self.skip && !self.queue.is_empty() {
            self.skip = false;
            let dropped = self.queue.len() as u64;
            self.queue.clear();
            return (None, dropped);
        }
        let keep = match self.mode {
            Present::Latest | Present::LatestVsync => 1,
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

    /// A picture's `present` took `took_us`, on a monitor refreshing every
    /// `refresh_us`.
    ///
    /// With [`Present::LatestVsync`], a present that waits means the one
    /// before wasn't on screen yet. Now and then that's the second of a
    /// pair, which is the point. But when frames arrive just before our
    /// refresh, every picture waits for it and goes out one refresh later
    /// than it could: the host's clock and ours drift, so that can last for
    /// many seconds. After [`CATCH_UP_AFTER`] waits in a row the next
    /// picture is left out: then nothing is waiting on the compositor when
    /// the one after arrives, and it goes out at once again.
    pub fn presented(&mut self, took_us: i64, refresh_us: i64) {
        if self.mode != Present::LatestVsync {
            return;
        }
        if took_us * WAITED_SHARE > refresh_us {
            self.waited += 1;
            if self.waited >= CATCH_UP_AFTER {
                self.waited = 0;
                self.skip = true;
            }
        } else {
            self.waited = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: i64 = 16_667;
    const NO_HOST: HostTiming = HostTiming {
        captured_us: 0,
        delivered_us: 0,
        submitted_us: 0,
        encoded_us: 0,
        sent_us: 0,
    };

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
        f.received(&NO_HOST, 0, 1_000);
        show(0, 1_000);
        // A frame missing at capture: 33 ms apart on the Mac already.
        f.received(&NO_HOST, 2 * FRAME, 1_000 + 2 * FRAME);
        show(2 * FRAME, 1_000 + 2 * FRAME);
        // One arrives but is replaced before it's shown.
        f.received(&NO_HOST, 3 * FRAME, 1_000 + 3 * FRAME);
        f.replaced(1);
        f.received(&NO_HOST, 4 * FRAME, 1_000 + 4 * FRAME);
        show(4 * FRAME, 1_000 + 4 * FRAME);
        // The next frame on time at capture, but shown late.
        f.received(&NO_HOST, 5 * FRAME, 1_000 + 6 * FRAME);
        show(5 * FRAME, 1_000 + 6 * FRAME);
        // Two shown within a few ms.
        f.received(&NO_HOST, 6 * FRAME, 1_000 + 6 * FRAME + 3_000);
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
        f.received(&NO_HOST, 7 * FRAME, 5_100_000);
        let lines = f.shown(&timing(7 * FRAME, 5_100_000));
        let summary = &lines[lines.len() - 2];
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
            f.received(&NO_HOST, i * 2 * FRAME, t);
            lines += f.shown(&timing(i * 2 * FRAME, t)).len();
            t += 2 * FRAME;
        }
        assert_eq!(lines, SLOW_LINES as usize);
        t += 5_000_000;
        let out = f.shown(&timing(1_000 * FRAME, t));
        assert!(out[out.len() - 2].contains("19 shown gaps > 25 ms: 19 missing"));
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

    #[test]
    fn pacing_names_the_stage_that_bunches() {
        let f = Frames::new(FRAME);
        // The client's clock is a different one: gaps don't care.
        let here = 7_000_000;
        let mut lines = Vec::new();
        for i in 0..20 {
            let captured = 1_000_000 + i * FRAME;
            let submitted = captured + 2_000;
            // Every fifth frame takes 28 ms to encode, the next one comes
            // out right behind it: a pair.
            let encoded = if i % 5 == 3 {
                captured + 28_000
            } else {
                captured + 12_000
            };
            let sent = encoded + 500;
            let host = HostTiming {
                captured_us: captured as u64,
                delivered_us: submitted as u64,
                submitted_us: submitted as u64,
                encoded_us: encoded as u64,
                sent_us: sent as u64,
            };
            let received = here + sent + 3_000;
            f.received(&host, i * FRAME, received);
            f.decoded(received + 2_000);
            lines.extend(f.shown(&timing(i * FRAME, received + 3_000)));
        }
        lines.extend(f.shown(&timing(20 * FRAME, here + 7_000_000)));
        let pacing = lines.last().unwrap();
        assert!(pacing.starts_with("  pacing, gap"), "{pacing}");
        assert!(pacing.contains("captured 16.7/16.7 [0]"), "{pacing}");
        assert!(pacing.contains("submitted 16.7/16.7 [0]"), "{pacing}");
        // Four pairs, 0.7 ms apart, and the gap of 32.7 ms before each.
        assert!(pacing.contains("encoded 16.7/32.7 [4]"), "{pacing}");
        assert!(pacing.contains("received 16.7/32.7 [4]"), "{pacing}");
        assert!(pacing.contains("decoded 16.7/32.7 [4]"), "{pacing}");
        assert!(
            pacing.contains("| most bunching starts in: encode (+4) |"),
            "{pacing}"
        );
        assert!(
            pacing.ends_with("present() ms median/p95/max ?, 0 waited"),
            "{pacing}"
        );
    }

    #[test]
    fn bunching_skips_points_without_stamps() {
        // An older host: no stamps of its own.
        let old = [None, None, None, None, None, Some(0), Some(3), Some(3)];
        assert_eq!(
            where_bunching_starts(&old),
            "most bunching starts in: decode queue + decode (+3)"
        );
        let gap = [Some(0), None, Some(2), Some(2), Some(1), Some(5)];
        assert_eq!(
            where_bunching_starts(&gap),
            "most bunching starts in: network (+4)"
        );
        let first_wins_a_tie = [Some(0), None, Some(2), Some(2), Some(4)];
        assert_eq!(
            where_bunching_starts(&first_wins_a_tie),
            "most bunching starts in: capture + encoder wait (+2)"
        );
        assert_eq!(
            where_bunching_starts(&[Some(1), Some(1), Some(0)]),
            "no stage bunches frames"
        );
    }

    #[test]
    fn present_modes_from_the_environment() {
        assert_eq!(Present::parse(None), Present::Latest);
        assert_eq!(Present::parse(Some("vsync")), Present::Vsync);
        assert_eq!(Present::parse(Some("latest-vsync ")), Present::LatestVsync);
        assert_eq!(Present::parse(Some("nonsense")), Present::Latest);
        assert!(!Present::Latest.vsync());
        assert!(Present::Vsync.vsync() && Present::LatestVsync.vsync());
    }

    #[test]
    fn latest_vsync_shows_only_the_newest() {
        let mut p = Pacer::new(Present::LatestVsync);
        p.push(1);
        assert_eq!(p.next(), (Some(1), 0));
        // Two came while that one waited for the compositor.
        p.presented(FRAME / 2, FRAME);
        p.push(2);
        p.push(3);
        assert_eq!(p.next(), (Some(3), 1));
        assert_eq!(p.next(), (None, 0));
    }

    #[test]
    fn latest_vsync_leaves_one_out_when_waiting_is_the_rule() {
        let mut p = Pacer::new(Present::LatestVsync);
        // Waits now and then (pairs) don't count up.
        for _ in 0..5 * CATCH_UP_AFTER {
            p.presented(FRAME / 3, FRAME);
            p.presented(500, FRAME);
        }
        p.push(0);
        assert_eq!(p.next(), (Some(0), 0));
        // Every present waits: after CATCH_UP_AFTER, one picture is left out.
        for i in 1..CATCH_UP_AFTER as i32 {
            p.push(i);
            assert_eq!(p.next(), (Some(i), 0));
            p.presented(FRAME / 4, FRAME);
        }
        p.push(100);
        assert_eq!(p.next(), (Some(100), 0));
        p.presented(FRAME / 4, FRAME);
        p.push(101);
        assert_eq!(p.next(), (None, 1), "left out");
        p.push(102);
        assert_eq!(p.next(), (Some(102), 0));
        // Other modes ignore how long presenting took.
        let mut v = Pacer::new(Present::Latest);
        for _ in 0..2 * CATCH_UP_AFTER {
            v.presented(FRAME, FRAME);
        }
        v.push(1);
        assert_eq!(v.next(), (Some(1), 0));
    }
}
