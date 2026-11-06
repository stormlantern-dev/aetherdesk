//! Frame accounting on the host (issue #109): how many frames each stage of
//! capture → encode → send let through in the last few seconds, and how
//! regular the capture was. One summary line every [`REPORT_EVERY_US`],
//! next to the existing "sent N frames" line, so a missing frame on the
//! client can be traced to the stage that lost it.
//!
//! Plain data and arithmetic, no macOS calls: the stages report into it
//! (the capture callback, the encoder, the forwarding thread) and it runs
//! on any platform, so its tests run on Linux too.

use std::sync::Mutex;

/// A capture gap longer than this (1.5 frames at 60 fps) is a missed frame.
pub const SLOW_US: i64 = 25_000;
/// Gaps longer than this mean the picture stood still, not a missed frame:
/// they're left out of the gap statistics.
pub const IDLE_US: i64 = 250_000;
/// How often the summary line comes, in microseconds.
pub const REPORT_EVERY_US: i64 = 5_000_000;

/// What the stages report into. Cheap: one short lock per event.
#[derive(Default)]
pub struct Pipeline {
    inner: Mutex<Window>,
}

/// The counts since the last summary.
#[derive(Default, Debug)]
struct Window {
    /// When this window started, host clock, µs. Zero: not yet.
    since_us: i64,
    /// ScreenCaptureKit callbacks with a picture.
    captured: u64,
    /// Its callbacks without one ("nothing changed").
    idle: u64,
    /// Captures dropped in front of the busy encoder.
    skipped: u64,
    encoded: u64,
    /// Frames the encoder took but gave nothing back for.
    encoder_dropped: u64,
    /// Encoded frames dropped because the send queue was full.
    queue_full: u64,
    forwarded: u64,
    last_capture_us: Option<i64>,
    capture_gaps: Vec<i64>,
    /// Delivered → submitted: waiting for the encoder.
    waits: Vec<i64>,
    /// Submitted → encoded.
    encodes: Vec<i64>,
}

/// Seconds on the host clock → µs.
fn us(seconds: f64) -> i64 {
    (seconds * 1e6).round() as i64
}

impl Pipeline {
    /// A picture came from ScreenCaptureKit, captured at `pts` (seconds).
    pub fn captured(&self, pts: f64) {
        let pts = us(pts);
        let mut w = self.inner.lock().unwrap();
        w.captured += 1;
        if let Some(last) = w.last_capture_us.replace(pts) {
            let gap = pts - last;
            if (0..IDLE_US).contains(&gap) {
                w.capture_gaps.push(gap);
            }
        }
    }

    /// A ScreenCaptureKit callback without a picture.
    pub fn idle(&self) {
        self.inner.lock().unwrap().idle += 1;
    }

    /// The busy encoder dropped a capture.
    pub fn skipped(&self) {
        self.inner.lock().unwrap().skipped += 1;
    }

    /// The encoder gave back nothing for a frame.
    pub fn encoder_dropped(&self) {
        self.inner.lock().unwrap().encoder_dropped += 1;
    }

    /// The encoder gave back a frame; times in seconds on the host clock.
    pub fn encoded(&self, delivered: f64, submitted: f64, encoded: f64) {
        let mut w = self.inner.lock().unwrap();
        w.encoded += 1;
        w.waits.push(us(submitted) - us(delivered));
        w.encodes.push(us(encoded) - us(submitted));
    }

    /// An encoded frame went on to be sent (`queued`), or was dropped because
    /// the send queue was full. Returns the summary line when one is due at
    /// `now` (host clock, seconds).
    pub fn forwarded(&self, queued: bool, now: f64) -> Option<String> {
        let now = us(now);
        let mut w = self.inner.lock().unwrap();
        if queued {
            w.forwarded += 1;
        } else {
            w.queue_full += 1;
        }
        if w.since_us == 0 {
            w.since_us = now;
            return None;
        }
        if now - w.since_us < REPORT_EVERY_US {
            return None;
        }
        let line = w.summary(now);
        // A new window, but the capture gap goes on across it.
        *w = Window {
            since_us: now,
            last_capture_us: w.last_capture_us,
            ..Window::default()
        };
        Some(line)
    }
}

/// Median, 95th percentile and maximum of `v`, in thousandths (µs → ms,
/// bytes → kB).
fn spread(v: &mut [i64]) -> String {
    if v.is_empty() {
        return "?".into();
    }
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
    format!("{:.1}/{:.1}/{:.1}", at(0.5), at(0.95), at(1.0))
}

/// QUIC's view of the path to the client, from `quinn::Connection::stats`.
/// The counters are totals since the connection started.
#[derive(Debug, Default, Clone, Copy)]
pub struct Path {
    pub rtt_us: i64,
    /// Congestion window: how much may be in flight, bytes.
    pub cwnd: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
}

/// What the sending side saw since its last line (#109): frame sizes, how
/// long handing a frame to QUIC took, and the path. A frame bigger than the
/// congestion window takes more than a round trip to go out, and the frame
/// behind it on the same stream waits: two then arrive close together.
#[derive(Default)]
pub struct Sent {
    frames: u64,
    sizes: Vec<i64>,
    writes: Vec<i64>,
    /// The path counters at the last line.
    last: Path,
}

impl Sent {
    /// A frame of `bytes` went to QUIC; the write took `write_us`.
    pub fn frame(&mut self, bytes: usize, write_us: i64) {
        self.frames += 1;
        self.sizes.push(bytes as i64);
        self.writes.push(write_us);
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Mbit/s over `secs`.
    pub fn mbps(&self, secs: f64) -> f64 {
        self.sizes.iter().sum::<i64>() as f64 * 8.0 / (secs * 1e6)
    }

    /// The part of the "sent" line about sizes and the path; starts over.
    pub fn line(&mut self, path: Path) -> String {
        let line = format!(
            "frame kB median/p95/max {}, write to QUIC ms {}, path: rtt {:.1} ms, \
             cwnd {} kB, {} packets lost, {} congestion events",
            spread(&mut self.sizes),
            spread(&mut self.writes),
            path.rtt_us as f64 / 1000.0,
            path.cwnd / 1000,
            path.lost_packets.saturating_sub(self.last.lost_packets),
            path.congestion_events
                .saturating_sub(self.last.congestion_events),
        );
        *self = Sent {
            last: path,
            ..Sent::default()
        };
        line
    }
}

impl Window {
    fn summary(&mut self, now_us: i64) -> String {
        let secs = (now_us - self.since_us) as f64 / 1e6;
        let long = self.capture_gaps.iter().filter(|&&g| g > SLOW_US).count();
        let slow_encodes = self.encodes.iter().filter(|&&e| e > 16_700).count();
        format!(
            "  frames in {secs:.1} s: {} captured ({} idle ticks), {} skipped (encoder busy), \
             {} encoded, {} dropped by the encoder, {} dropped (send queue full), {} sent on \
             | ms median/p95/max: capture gap {} ({long} > 25 ms), encoder wait {}, \
             encode {} ({slow_encodes} > 16.7 ms)",
            self.captured,
            self.idle,
            self.skipped,
            self.encoded,
            self.encoder_dropped,
            self.queue_full,
            self.forwarded,
            spread(&mut self.capture_gaps),
            spread(&mut self.waits),
            spread(&mut self.encodes),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_stages_and_reports_every_five_seconds() {
        let p = Pipeline::default();
        // First call opens the window.
        assert_eq!(p.forwarded(true, 100.0), None);
        let frame = 1.0 / 60.0;
        for i in 0..10 {
            // Every 16.7 ms, once 4.2 ms late.
            let pts = 100.0 + i as f64 * frame + if i >= 5 { 1.0 / 240.0 } else { 0.0 };
            p.captured(pts);
            p.encoded(pts, pts + 0.001, pts + 0.012);
        }
        p.idle();
        p.skipped();
        p.encoder_dropped();
        assert_eq!(p.forwarded(false, 102.0), None);
        let line = p.forwarded(true, 105.0).expect("5 s passed");
        assert!(line.contains("frames in 5.0 s"), "{line}");
        assert!(line.contains("10 captured (1 idle ticks)"), "{line}");
        assert!(line.contains("1 skipped (encoder busy)"), "{line}");
        assert!(
            line.contains("10 encoded, 1 dropped by the encoder"),
            "{line}"
        );
        assert!(
            line.contains("1 dropped (send queue full), 2 sent on"),
            "{line}"
        );
        // Nine gaps: eight of 16.7 ms, one of 20.8.
        assert!(
            line.contains("capture gap 16.7/20.8/20.8 (0 > 25 ms)"),
            "{line}"
        );
        assert!(line.contains("encoder wait 1.0/1.0/1.0"), "{line}");
        assert!(
            line.contains("encode 11.0/11.0/11.0 (0 > 16.7 ms)"),
            "{line}"
        );
        // The next window starts empty.
        let next = p.forwarded(true, 110.0).expect("another 5 s");
        assert!(next.contains("0 captured"), "{next}");
        assert!(next.contains("capture gap ?"), "{next}");
    }

    #[test]
    fn sent_line_has_sizes_and_path_changes() {
        let mut s = Sent::default();
        for (bytes, us) in [(40_000, 100), (60_000, 200), (250_000, 4_000)] {
            s.frame(bytes, us);
        }
        assert_eq!(s.frames(), 3);
        assert!((s.mbps(1.0) - 2.8).abs() < 1e-9);
        let path = Path {
            rtt_us: 2_500,
            cwnd: 120_000,
            lost_packets: 7,
            congestion_events: 2,
        };
        let line = s.line(path);
        assert_eq!(
            line,
            "frame kB median/p95/max 60.0/250.0/250.0, write to QUIC ms 0.2/4.0/4.0, \
             path: rtt 2.5 ms, cwnd 120 kB, 7 packets lost, 2 congestion events"
        );
        // The next line counts from here.
        let line = s.line(Path {
            lost_packets: 9,
            congestion_events: 2,
            ..path
        });
        assert!(line.starts_with("frame kB median/p95/max ?, write to QUIC ms ?"));
        assert!(
            line.ends_with("2 packets lost, 0 congestion events"),
            "{line}"
        );
        assert_eq!(s.frames(), 0);
    }

    #[test]
    fn a_still_picture_is_no_gap() {
        let p = Pipeline::default();
        p.forwarded(true, 0.5);
        p.captured(1.0);
        p.captured(3.0); // two seconds without change
        p.captured(3.030); // a missed frame
        let line = p.forwarded(true, 6.0).unwrap();
        assert!(
            line.contains("capture gap 30.0/30.0/30.0 (1 > 25 ms)"),
            "{line}"
        );
    }
}
