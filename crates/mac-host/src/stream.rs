//! One capture → encode pipeline and one input injector: what a session
//! needs on the machine itself. Shared by `serve` (all in one process) and
//! the agent (driven by the daemon).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

use aetherdesk_proto::{HostTiming, InputEvent, VideoFrame};
use anyhow::anyhow;
use tokio::sync::mpsc::error::TrySendError;

use crate::capture::{Capture, Display};
use crate::encoder::{EncodedFrame, Encoder, Settings, host_time};
use crate::inject;
use crate::input::Rect;

/// What a client may tune. The display decides the size.
#[derive(Debug, Clone, Copy)]
pub struct StreamSettings {
    pub fps: u32,
    pub mbps: u32,
    pub in_flight: usize,
}

/// Frames waiting to be sent. Beyond this the client or network is too slow:
/// we drop frames and ask the encoder for a keyframe to resync.
const SEND_QUEUE: usize = 8;

/// A running capture of one display.
pub struct Stream {
    pub capture: Capture,
    /// Encoded frames (or a fatal error), oldest first, with `sent_us` set
    /// to when they left the encoder queue.
    pub frames: tokio::sync::mpsc::Receiver<Result<VideoFrame, String>>,
    /// Set to get a keyframe next.
    pub keyframe: Arc<AtomicBool>,
    /// Captures skipped because the encoder was busy.
    pub skipped: Arc<AtomicU64>,
    pub width: u32,
    pub height: u32,
}

impl Stream {
    pub fn start(display: &Display, s: &StreamSettings) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let settings = Settings {
            width: display.width,
            height: display.height,
            fps: s.fps,
            bitrate_bps: s.mbps.saturating_mul(1_000_000),
            max_in_flight: s.in_flight,
        };
        let encoder = Encoder::new(&settings, tx.clone()).map_err(anyhow::Error::msg)?;
        let keyframe = encoder.keyframe_trigger();
        let skipped = encoder.skipped_counter();
        let capture = Capture::start(display, s.fps, encoder, tx).map_err(anyhow::Error::msg)?;
        Ok(Self {
            capture,
            frames: forward_frames(rx, keyframe.clone()),
            keyframe,
            skipped,
            width: display.width as u32,
            height: display.height as u32,
        })
    }

    pub fn stop(self) -> anyhow::Result<()> {
        self.capture.stop().map_err(|e| anyhow!(e))
    }
}

/// Moves encoder output from its std channel onto a small bounded tokio
/// channel, as protocol frames. When that's full the network is behind: drop
/// the frame and force a keyframe, since every following P-frame depends on
/// the lost one.
fn forward_frames(
    rx: mpsc::Receiver<Result<EncodedFrame, String>>,
    keyframe: Arc<AtomicBool>,
) -> tokio::sync::mpsc::Receiver<Result<VideoFrame, String>> {
    let (tx, out) = tokio::sync::mpsc::channel(SEND_QUEUE);
    std::thread::spawn(move || {
        let mut first_pts = None;
        for item in rx {
            match item {
                Ok(frame) => match tx.try_send(Ok(video_frame(frame, &mut first_pts))) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => keyframe.store(true, Ordering::Relaxed),
                    Err(TrySendError::Closed(_)) => break,
                },
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                }
            }
        }
    });
    out
}

/// Turns encoder output into a protocol frame. `first_pts` is the stream's
/// first capture time, for `pts_us`; `sent_us` is now.
fn video_frame(frame: EncodedFrame, first_pts: &mut Option<f64>) -> VideoFrame {
    let base = *first_pts.get_or_insert(frame.pts);
    VideoFrame {
        pts_us: micros(frame.pts - base),
        keyframe: frame.keyframe,
        timing: HostTiming {
            captured_us: micros(frame.pts),
            encoded_us: micros(frame.encoded_at),
            sent_us: micros(host_time()),
        },
        data: frame.data,
    }
}

/// Seconds on the host clock → whole microseconds.
pub fn micros(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1e6) as u64
}

/// Starts an injector thread for a display with the given bounds. It runs
/// until the returned sender is dropped, then releases held keys and buttons.
pub fn spawn_injector(bounds: Rect) -> mpsc::Sender<InputEvent> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::spawn(move || inject::run(bounds, rx));
    tx
}
