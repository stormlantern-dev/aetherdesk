//! Where a frame's time goes, from capture on the host to `present` here.
//!
//! The host stamps each frame on its own clock ([`HostTiming`]). Pings on
//! the control stream tell us how that clock relates to ours: the host's
//! answer was written about half a round trip before we read it. We keep
//! the estimate from the fastest recent ping, which has the least room for
//! error, and convert host stamps to our clock with it.
//!
//! Not measured: what happens before capture (the Mac compositing its
//! screen) and after `present` (Hyprland compositing and the monitor).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use aetherdesk_proto::HostTiming;

/// Microseconds since this process started, on a monotonic clock.
pub fn now_us() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as i64
}

/// How many recent pings to pick the fastest from.
const PINGS: usize = 10;

/// Host clock → our clock, from ping/pong pairs.
#[derive(Default)]
pub struct ClockSync {
    /// (round trip, host time minus our time), newest last.
    samples: Mutex<Vec<(i64, i64)>>,
}

impl ClockSync {
    /// Records a pong that answered a ping sent at `sent_us` and arrived at
    /// `received_us` (our clock), stamped `host_us` by the host.
    pub fn pong(&self, sent_us: i64, host_us: u64, received_us: i64) {
        let rtt = received_us - sent_us;
        if rtt < 0 {
            return;
        }
        let offset = host_us as i64 - (sent_us + rtt / 2);
        let mut samples = self.samples.lock().unwrap();
        samples.push((rtt, offset));
        if samples.len() > PINGS {
            samples.remove(0);
        }
    }

    /// Forgets all samples, e.g. after reconnecting to a host that may have
    /// restarted (its clock can be a different one).
    pub fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }

    /// The fastest recent ping: (round trip, host minus ours), if any.
    fn best(&self) -> Option<(i64, i64)> {
        self.samples.lock().unwrap().iter().copied().min()
    }

    /// Round trip of the fastest recent ping, in microseconds.
    pub fn rtt_us(&self) -> Option<i64> {
        self.best().map(|(rtt, _)| rtt)
    }

    /// A host clock time on our clock.
    pub fn to_local(&self, host_us: u64) -> Option<i64> {
        self.best().map(|(_, offset)| host_us as i64 - offset)
    }
}

/// A frame's timestamps, all on our clock, in microseconds. Zero when not
/// (yet) known.
#[derive(Debug, Default, Clone, Copy)]
pub struct Timing {
    pub captured: i64,
    pub encoded: i64,
    pub sent: i64,
    pub received: i64,
    pub decode_start: i64,
    pub decoded: i64,
    pub presented: i64,
}

impl Timing {
    /// Host stamps converted with `sync`, plus our arrival time.
    pub fn arrived(host: &HostTiming, sync: &ClockSync, received: i64) -> Self {
        let local = |t: u64| sync.to_local(t).unwrap_or(0);
        Self {
            captured: local(host.captured_us),
            encoded: local(host.encoded_us),
            sent: local(host.sent_us),
            received,
            ..Self::default()
        }
    }
}

/// The two timestamps a stage lies between.
type Span = fn(&Timing) -> (i64, i64);

/// Stage names and their spans.
const STAGES: [(&str, Span); 6] = [
    ("encode", |t| (t.captured, t.encoded)),
    ("host queue", |t| (t.encoded, t.sent)),
    ("network", |t| (t.sent, t.received)),
    ("decode queue", |t| (t.received, t.decode_start)),
    ("decode", |t| (t.decode_start, t.decoded)),
    ("to screen", |t| (t.decoded, t.presented)),
];

/// Collects shown frames and prints a breakdown every few seconds.
pub struct Stats {
    frames: Vec<Timing>,
    next: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            next: Instant::now() + REPORT_EVERY,
        }
    }
}

const REPORT_EVERY: Duration = Duration::from_secs(5);

impl Stats {
    pub fn shown(&mut self, timing: Timing, sync: &ClockSync) {
        self.frames.push(timing);
        if Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + REPORT_EVERY;
        let rtt = sync.rtt_us();
        println!("{}", report(&std::mem::take(&mut self.frames), rtt));
    }
}

/// One line: the median of every stage and of the total, in milliseconds.
/// Host stages are missing until the first pong has arrived.
fn report(frames: &[Timing], rtt_us: Option<i64>) -> String {
    let median = |f: &dyn Fn(&Timing) -> (i64, i64)| {
        let mut v: Vec<i64> = frames
            .iter()
            .map(f)
            .filter(|&(a, b)| a != 0 && b != 0)
            .map(|(a, b)| b - a)
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2] as f64 / 1000.0)
    };
    let ms = |v: Option<f64>| v.map_or("?".to_string(), |v| format!("{v:.1}"));
    let mut parts: Vec<String> = STAGES
        .iter()
        .map(|(name, span)| format!("{name} {}", ms(median(span))))
        .collect();
    parts.push(format!(
        "total {}",
        ms(median(&|t: &Timing| (t.captured, t.presented)))
    ));
    format!(
        "  latency ms (median of {} frames): {} | rtt {}",
        frames.len(),
        parts.join(", "),
        ms(rtt_us.map(|r| r as f64 / 1000.0))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sync_uses_the_fastest_ping() {
        let sync = ClockSync::default();
        // Host clock runs 1 s ahead. A slow ping, then a fast one.
        sync.pong(0, 1_000_000 + 4_000, 10_000); // rtt 10 ms, answered late
        sync.pong(20_000, 1_000_000 + 21_000, 22_000); // rtt 2 ms, symmetric
        assert_eq!(sync.rtt_us(), Some(2_000));
        assert_eq!(sync.to_local(1_000_000 + 50_000), Some(50_000));
    }

    #[test]
    fn unknown_clock_leaves_host_stages_empty() {
        let sync = ClockSync::default();
        let t = Timing::arrived(
            &HostTiming {
                captured_us: 5,
                encoded_us: 6,
                sent_us: 7,
            },
            &sync,
            100,
        );
        assert_eq!((t.captured, t.sent, t.received), (0, 0, 100));
        let line = report(&[t], None);
        assert!(line.contains("network ?"), "{line}");
        assert!(line.contains("rtt ?"), "{line}");
    }

    #[test]
    fn report_takes_medians() {
        let frame = |d: i64| Timing {
            captured: 1_000,
            encoded: 1_000 + d,
            sent: 2_000 + d,
            received: 3_000 + d,
            decode_start: 3_000 + d,
            decoded: 4_000 + d,
            presented: 5_000 + d,
        };
        let line = report(&[frame(1_000), frame(3_000), frame(2_000)], Some(2_500));
        assert!(line.contains("encode 2.0"), "{line}");
        assert!(line.contains("network 1.0"), "{line}");
        assert!(line.contains("total 6.0"), "{line}");
        assert!(line.contains("rtt 2.5"), "{line}");
    }
}
