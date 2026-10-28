//! `aether-agent record`: capture + encode for a few seconds and write the raw
//! H.264 stream to a file, to check the Mac side on its own.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

use crate::capture::{self, Capture};
use crate::encoder::{DEFAULT_IN_FLIGHT, Encoder, Settings};

pub const USAGE: &str = "\
usage: aether-agent record [--seconds N] [--out PATH] [--fps N] [--mbps N]

  --seconds N   how long to record (default 10)
  --out PATH    output file, raw H.264 Annex B (default /tmp/aetherdesk.h264)
  --fps N       maximum frame rate (default 60)
  --mbps N      average bitrate in Mbit/s (default 20)";

struct Opts {
    seconds: u64,
    out: PathBuf,
    fps: u32,
    mbps: u32,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        seconds: 10,
        out: "/tmp/aetherdesk.h264".into(),
        fps: 60,
        mbps: 20,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        let num = |v: String| v.parse::<u32>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--seconds" => o.seconds = u64::from(num(val()?)?),
            "--out" => o.out = val()?.into(),
            "--fps" => o.fps = num(val()?)?.max(1),
            "--mbps" => o.mbps = num(val()?)?.max(1),
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown option {flag}\n\n{USAGE}")),
        }
    }
    Ok(o)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        return Err(
            "no Screen Recording permission. Grant it to the app you run this from \
                    (e.g. Terminal) in System Settings → Privacy & Security → Screen & System \
                    Audio Recording, restart that app, and try again."
                .into(),
        );
    }

    let display = capture::main_display()?;
    println!(
        "recording main display {}x{} px for {} s at up to {} fps, {} Mbit/s → {}",
        display.width,
        display.height,
        o.seconds,
        o.fps,
        o.mbps,
        o.out.display()
    );

    let (tx, rx) = mpsc::channel();
    let settings = Settings {
        width: display.width,
        height: display.height,
        fps: o.fps,
        bitrate_bps: o.mbps.saturating_mul(1_000_000),
        max_in_flight: DEFAULT_IN_FLIGHT,
    };
    let encoder = Encoder::new(&settings, tx.clone())?;
    let capture = Capture::start(&display, o.fps, encoder, tx)?;

    let file = File::create(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    let mut file = BufWriter::new(file);
    let mut stats = Stats::default();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(o.seconds);
    let mut next_report = start + Duration::from_secs(1);

    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(Ok(frame)) => {
                stats.add(frame.pts, frame.keyframe, frame.data.len());
                file.write_all(&frame.data)
                    .map_err(|e| format!("writing: {e}"))?;
            }
            Ok(Err(e)) => {
                let _ = capture.stop();
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if Instant::now() >= next_report {
            println!("  {}", stats.line());
            next_report += Duration::from_secs(1);
        }
    }

    capture.stop()?;
    // Frames the encoder flushed on stop.
    while let Ok(Ok(frame)) = rx.try_recv() {
        stats.add(frame.pts, frame.keyframe, frame.data.len());
        file.write_all(&frame.data)
            .map_err(|e| format!("writing: {e}"))?;
    }
    file.flush().map_err(|e| format!("writing: {e}"))?;

    println!("done: {}", stats.line());
    if stats.frames == 0 {
        return Err("no frames encoded — is the display asleep?".into());
    }
    println!(
        "\nThe screen only sends frames when something changes, so the file has no fixed \
         frame rate. To watch it:\n  ffplay -f h264 {0}\nor make an MP4 for QuickTime:\n  \
         ffmpeg -f h264 -i {0} -c copy /tmp/aetherdesk.mp4",
        o.out.display()
    );
    Ok(())
}

#[derive(Default)]
struct Stats {
    frames: u64,
    keyframes: u64,
    bytes: u64,
    first_pts: Option<f64>,
    last_pts: f64,
}

impl Stats {
    fn add(&mut self, pts: f64, keyframe: bool, bytes: usize) {
        self.frames += 1;
        self.keyframes += u64::from(keyframe);
        self.bytes += bytes as u64;
        self.first_pts.get_or_insert(pts);
        self.last_pts = pts;
    }

    fn line(&self) -> String {
        let span = self.first_pts.map_or(0.0, |f| self.last_pts - f);
        let fps = if span > 0.0 {
            (self.frames - 1) as f64 / span
        } else {
            0.0
        };
        format!(
            "{} frames ({} keyframes), {:.1} fps, {:.1} MB",
            self.frames,
            self.keyframes,
            fps,
            self.bytes as f64 / 1e6
        )
    }
}
