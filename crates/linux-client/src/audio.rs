//! The Mac's sound: Opus packets come in from the network thread, ffmpeg
//! decodes them and SDL plays them (through PipeWire). A buffer of about
//! [`TARGET_MS`] evens out network jitter. The sound card and the Mac
//! count time a little differently, so playback speeds up or slows down by
//! at most half a percent to keep that buffer where it should be, instead
//! of letting it run dry or grow.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use aetherdesk_proto::{AUDIO_CHANNELS, AUDIO_FRAMES, AUDIO_RATE, AudioPacket};
use anyhow::{Context, anyhow, bail};
use ffmpeg_next as ffmpeg;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};

use crate::latency::{ClockSync, now_us};
use sdl3::sys::audio as sys;

/// How much sound we keep queued by default, in ms: enough for Wi-Fi
/// jitter. `AETHERDESK_AUDIO_MS` sets another amount (10–500).
pub const TARGET_MS: u32 = 30;

/// Beyond the target, how much may pile up (after a burst, or a stall on
/// our side) before we throw packets away to catch up.
const SLACK_MS: u32 = 60;

/// A gap of at most this many lost packets is filled with silence, so what
/// follows stays in time. Longer gaps: the buffer refills instead.
const FILL_LOST: u32 = 5;

/// How fast the measured buffer level follows the queue, per packet (100 a
/// second): about half a second.
const SMOOTHING: f64 = 0.02;

/// The most playback speeds up or slows down: 0.5 %, too little to hear.
const MAX_SKEW: f64 = 0.005;

/// What Opus itself delays, as macOS encodes it (312 frames of look-ahead,
/// measured on the Mac): the decoder's output runs this far behind the
/// packets' capture times.
const CODEC_DELAY_US: i64 = 6_500;

fn ms_to_frames(ms: u32) -> u32 {
    AUDIO_RATE / 1000 * ms
}

fn frames_to_ms(frames: f64) -> f64 {
    frames * 1000.0 / AUDIO_RATE as f64
}

/// The buffer the user asked for, or [`TARGET_MS`].
pub fn target_ms() -> u32 {
    std::env::var("AETHERDESK_AUDIO_MS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(TARGET_MS, |ms| ms.clamp(10, 500))
}

/// Counters for the log and the test hooks.
#[derive(Debug, Default)]
pub struct Stats {
    pub packets: AtomicU64,
    pub lost: AtomicU64,
    pub dropped: AtomicU64,
    pub underruns: AtomicU64,
    /// Frames handed to SDL.
    pub played: AtomicU64,
}

/// Opus → interleaved stereo `f32`, with ffmpeg.
pub struct OpusDecoder {
    inner: ffmpeg::decoder::Audio,
    frame: ffmpeg::frame::Audio,
}

impl OpusDecoder {
    pub fn new() -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::OPUS)
            .context("this ffmpeg can't decode Opus")?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        // The packets come without a header: say what they are.
        unsafe {
            let p = ctx.as_mut_ptr();
            (*p).sample_rate = AUDIO_RATE as i32;
            ffmpeg::ffi::av_channel_layout_default(&mut (*p).ch_layout, AUDIO_CHANNELS as i32);
        }
        Ok(Self {
            inner: ctx.decoder().audio()?,
            frame: ffmpeg::frame::Audio::empty(),
        })
    }

    /// Decodes one packet and appends its sound to `out`, left and right
    /// alternating.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> anyhow::Result<()> {
        self.inner
            .send_packet(&ffmpeg::Packet::copy(data))
            .map_err(|e| anyhow!("decoding sound: {e}"))?;
        while self.inner.receive_frame(&mut self.frame).is_ok() {
            let f = &self.frame;
            if f.format() != ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar)
                || f.planes() < 2
            {
                bail!("unexpected sound format {:?} from the decoder", f.format());
            }
            let n = f.samples();
            let (l, r) = (&f.plane::<f32>(0)[..n], &f.plane::<f32>(1)[..n]);
            out.extend(l.iter().zip(r).flat_map(|(&l, &r)| [l, r]));
        }
        Ok(())
    }
}

/// How a packet relates to the ones before it.
#[derive(Debug, PartialEq, Eq)]
enum Arrival {
    Next,
    /// After this many lost ones.
    AfterLoss(u32),
    /// Older than one already played: too late.
    Late,
    /// Numbering started over: the host restarted its capture.
    Restart,
}

#[derive(Debug, Default)]
struct Sequence {
    next: Option<u32>,
    /// Capture time of the newest packet so far.
    newest_us: u64,
}

impl Sequence {
    /// How far apart counts as a restart rather than loss or reordering.
    const WINDOW: u32 = 50;

    /// A packet numbered `seq`, captured at `captured_us` on the host.
    fn arrive(&mut self, seq: u32, captured_us: u64) -> Arrival {
        let newer = captured_us > self.newest_us;
        let Some(next) = self.next else {
            self.next = Some(seq.wrapping_add(1));
            self.newest_us = captured_us;
            return Arrival::Next;
        };
        let ahead = seq.wrapping_sub(next);
        let behind = next.wrapping_sub(seq);
        let arrival = match ahead {
            0 => Arrival::Next,
            n if n <= Self::WINDOW => Arrival::AfterLoss(n),
            // An old number with a new time: counting started over.
            _ if behind <= Self::WINDOW && !newer => return Arrival::Late,
            _ => Arrival::Restart,
        };
        self.next = Some(seq.wrapping_add(1));
        self.newest_us = self.newest_us.max(captured_us);
        arrival
    }
}

/// What to do with the next decoded packet.
#[derive(Debug, PartialEq)]
struct Step {
    /// Queue it (false: drop it, we're too far behind).
    put: bool,
    /// Start playing: the buffer is full enough.
    resume: bool,
    /// Stop until the buffer has filled again: it ran dry.
    pause: bool,
    /// Playback speed: above 1 plays faster and drains the buffer.
    ratio: f64,
}

/// Keeps the queue near its target, all in frames.
#[derive(Debug)]
struct Playout {
    target: u32,
    playing: bool,
    /// Smoothed queue level after a put, once playing.
    level: Option<f64>,
}

impl Playout {
    fn new(target: u32) -> Self {
        Self {
            target,
            playing: false,
            level: None,
        }
    }

    /// `queued`: frames waiting in SDL now; `incoming`: the packet's frames.
    fn step(&mut self, queued: u32, incoming: u32) -> Step {
        let mut step = Step {
            put: true,
            resume: false,
            pause: false,
            ratio: 1.0,
        };
        if self.playing && queued == 0 {
            self.playing = false;
            self.level = None;
            step.pause = true;
        }
        if !self.playing {
            if queued + incoming >= self.target {
                self.playing = true;
                step.resume = true;
            }
            return step;
        }
        if queued > self.target + ms_to_frames(SLACK_MS) {
            step.put = false;
        }
        let after = f64::from(queued + if step.put { incoming } else { 0 });
        let level = match self.level {
            Some(l) => l + (after - l) * SMOOTHING,
            None => after,
        };
        self.level = Some(level);
        // 0.02 % faster per ms too much, at most MAX_SKEW.
        let error_ms = frames_to_ms(level - f64::from(self.target + incoming / 2));
        step.ratio = 1.0 + (error_ms * 0.0002).clamp(-MAX_SKEW, MAX_SKEW);
        step
    }
}

/// An SDL stream pointer for the player thread. SDL's audio stream calls
/// are safe from any thread; the owner stays with the window, which joins
/// the thread before dropping it.
struct RawStream(*mut sys::SDL_AudioStream);
unsafe impl Send for RawStream {}

/// Plays the Mac's sound while it lives.
pub struct Player {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    pub stats: Arc<Stats>,
    // Last: dropped after the thread is joined (see Drop).
    _stream: AudioStreamOwner,
}

impl Player {
    pub fn start(
        sdl: &sdl3::Sdl,
        packets: mpsc::Receiver<AudioPacket>,
        target_ms: u32,
        sync: Arc<ClockSync>,
    ) -> anyhow::Result<Self> {
        // Small device buffers: the queue below is the jitter buffer.
        sdl3::hint::set("SDL_AUDIO_DEVICE_SAMPLE_FRAMES", &AUDIO_FRAMES.to_string());
        let audio = sdl.audio().map_err(|e| anyhow!("SDL audio: {e}"))?;
        let spec = AudioSpec {
            freq: Some(AUDIO_RATE as i32),
            channels: Some(AUDIO_CHANNELS as i32),
            format: Some(AudioFormat::f32_sys()),
        };
        let stream = audio
            .default_playback_device()
            .open_device_stream(Some(&spec))
            .map_err(|e| anyhow!("opening the sound output: {e}"))?;
        let mut decoder = OpusDecoder::new()?;
        let stats = Arc::new(Stats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let raw = RawStream(stream.raw());
        let (st, stop2) = (stats.clone(), stop.clone());
        let target = ms_to_frames(target_ms);
        say!(
            "sound: playing through {} with {target_ms} ms of buffer",
            audio.current_audio_driver()
        );
        let thread = thread::Builder::new().name("audio".into()).spawn(move || {
            let raw = raw;
            let player = (&packets, target, &*st, &*stop2, &*sync);
            if let Err(e) = play(raw.0, &mut decoder, player) {
                warn!("sound stopped: {e:#}");
            }
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
            stats,
            _stream: stream,
        })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let s = &self.stats;
        say!(
            "sound: {} packets, {} lost, {} dropped, {} underruns",
            s.packets.load(Ordering::Relaxed),
            s.lost.load(Ordering::Relaxed),
            s.dropped.load(Ordering::Relaxed),
            s.underruns.load(Ordering::Relaxed)
        );
    }
}

fn sdl_error() -> String {
    sdl3::get_error().to_string()
}

/// The player thread: until `stop`, or the packets end.
fn play(
    stream: *mut sys::SDL_AudioStream,
    decoder: &mut OpusDecoder,
    (packets, target, stats, stop, sync): (
        &mpsc::Receiver<AudioPacket>,
        u32,
        &Stats,
        &AtomicBool,
        &ClockSync,
    ),
) -> anyhow::Result<()> {
    let put = |samples: &[f32]| -> anyhow::Result<()> {
        let ok = unsafe {
            sys::SDL_PutAudioStreamData(
                stream,
                samples.as_ptr().cast(),
                std::mem::size_of_val(samples) as i32,
            )
        };
        if !ok {
            bail!("queueing sound: {}", sdl_error());
        }
        stats
            .played
            .fetch_add((samples.len() / 2) as u64, Ordering::Relaxed);
        Ok(())
    };
    let bytes_per_frame = 4 * AUDIO_CHANNELS as i32;
    let mut sequence = Sequence::default();
    let mut playout = Playout::new(target);
    let mut pcm = Vec::with_capacity(2 * AUDIO_FRAMES as usize);
    let silence = vec![0.0f32; 2 * AUDIO_FRAMES as usize];
    let mut next_report = Instant::now() + Duration::from_secs(5);
    let mut last = [0u64; 4];
    let mut skew = 1.0;
    // Per packet: from capture on the Mac to leaving SDL's queue for the
    // sound output, in µs.
    let mut delays: Vec<i64> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let packet = match packets.recv_timeout(Duration::from_millis(100)) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        stats.packets.fetch_add(1, Ordering::Relaxed);
        match sequence.arrive(packet.seq, packet.captured_us) {
            Arrival::Next => {}
            Arrival::Late => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Arrival::AfterLoss(n) => {
                stats.lost.fetch_add(u64::from(n), Ordering::Relaxed);
                if n <= FILL_LOST {
                    for _ in 0..n {
                        put(&silence)?;
                    }
                }
            }
            Arrival::Restart => *decoder = OpusDecoder::new()?,
        }
        pcm.clear();
        if let Err(e) = decoder.decode(&packet.data, &mut pcm) {
            warn!("{e:#}");
            continue;
        }
        let queued = unsafe { sys::SDL_GetAudioStreamQueued(stream) }.max(0) / bytes_per_frame;
        let step = playout.step(queued as u32, (pcm.len() / 2) as u32);
        if step.pause {
            stats.underruns.fetch_add(1, Ordering::Relaxed);
            unsafe { sys::SDL_PauseAudioStreamDevice(stream) };
        }
        if step.put {
            // It plays once everything queued before it has: at the
            // current speed, that many frames from now.
            if let Some(captured) = sync.to_local(packet.captured_us) {
                let wait_us = f64::from(queued as u32) * 1e6 / f64::from(AUDIO_RATE) / skew;
                delays.push(now_us() + wait_us as i64 + CODEC_DELAY_US - captured);
            }
            put(&pcm)?;
        } else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
        if step.resume {
            unsafe { sys::SDL_ResumeAudioStreamDevice(stream) };
        }
        if (step.ratio - skew).abs() > 0.0001 || step.resume {
            skew = step.ratio;
            unsafe { sys::SDL_SetAudioStreamFrequencyRatio(stream, skew as f32) };
        }
        if Instant::now() >= next_report {
            let now = [
                stats.packets.load(Ordering::Relaxed),
                stats.lost.load(Ordering::Relaxed),
                stats.dropped.load(Ordering::Relaxed),
                stats.underruns.load(Ordering::Relaxed),
            ];
            let d: Vec<u64> = now.iter().zip(&last).map(|(a, b)| a - b).collect();
            say!(
                "{}",
                report(
                    &mut delays,
                    frames_to_ms(f64::from(queued as u32)),
                    skew,
                    [d[0], d[1], d[2], d[3]]
                )
            );
            delays.clear();
            last = now;
            next_report += Duration::from_secs(5);
        }
    }
    Ok(())
}

/// One line every 5 s, like the picture's latency line: the median delay
/// from capture on the Mac to the sound output here, the buffer, and what
/// went wrong, if anything.
fn report(
    delays: &mut [i64],
    buffer_ms: f64,
    skew: f64,
    [packets, lost, dropped, underruns]: [u64; 4],
) -> String {
    let delay = if delays.is_empty() {
        "?".to_string()
    } else {
        delays.sort_unstable();
        format!("{:.1}", delays[delays.len() / 2] as f64 / 1000.0)
    };
    let mut line = format!(
        "  sound ms (median of {} packets): Mac to output {delay} (buffer {buffer_ms:.0}), speed {:+.2} %",
        delays.len(),
        (skew - 1.0) * 100.0
    );
    if lost + dropped + underruns > 0 {
        line +=
            &format!(" | {packets} packets: {lost} lost, {dropped} dropped, {underruns} underruns");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_takes_the_median_and_names_trouble() {
        let mut d = vec![40_000, 20_000, 30_000];
        let line = report(&mut d, 28.4, 1.001, [500, 0, 0, 0]);
        assert!(line.contains("median of 3 packets"), "{line}");
        assert!(line.contains("Mac to output 30.0 (buffer 28)"), "{line}");
        assert!(line.contains("speed +0.10 %"), "{line}");
        assert!(!line.contains("lost"), "{line}");
        let line = report(&mut [], 0.0, 1.0, [500, 3, 1, 0]);
        assert!(line.contains("Mac to output ?"), "{line}");
        assert!(line.contains("3 lost, 1 dropped, 0 underruns"), "{line}");
    }

    #[test]
    fn sequence_tells_loss_from_restart() {
        // Packet n is captured at n × 10 ms.
        let mut s = Sequence::default();
        let at = |s: &mut Sequence, n: u32| s.arrive(n, u64::from(n) * 10_000);
        assert_eq!(at(&mut s, 7), Arrival::Next);
        assert_eq!(at(&mut s, 8), Arrival::Next);
        assert_eq!(at(&mut s, 11), Arrival::AfterLoss(2));
        // 9 shows up after all: too late, and it doesn't move us back.
        assert_eq!(at(&mut s, 9), Arrival::Late);
        assert_eq!(at(&mut s, 12), Arrival::Next);
        // A new capture counts from 0 again, with newer times.
        assert_eq!(s.arrive(0, 500_000), Arrival::Restart);
        assert_eq!(s.arrive(1, 510_000), Arrival::Next);
        let mut w = Sequence::default();
        w.arrive(u32::MAX, 1);
        assert_eq!(w.arrive(0, 2), Arrival::Next);
    }

    #[test]
    fn playout_fills_then_plays_then_refills() {
        let target = ms_to_frames(30);
        let mut p = Playout::new(target);
        // Filling: queue, don't start yet.
        let s = p.step(0, 480);
        assert!(s.put && !s.resume);
        let s = p.step(960, 480);
        assert!(s.put && s.resume, "starts at 30 ms: {s:?}");
        // Ran dry: pause until full again.
        let s = p.step(0, 480);
        assert!(s.pause && s.put && !s.resume);
        assert!(p.step(960, 480).resume);
    }

    #[test]
    fn playout_steers_toward_the_target() {
        let target = ms_to_frames(30);
        let mut p = Playout::new(target);
        p.step(target, 480);
        // Too much queued for a while: play faster, but not by much.
        let mut s = p.step(target, 480);
        for _ in 0..500 {
            s = p.step(target + ms_to_frames(40), 480);
        }
        assert!(s.ratio > 1.0 && s.ratio <= 1.0 + MAX_SKEW, "{s:?}");
        // Too little: slower.
        for _ in 0..500 {
            s = p.step(ms_to_frames(10), 480);
        }
        assert!(s.ratio < 1.0 && s.ratio >= 1.0 - MAX_SKEW, "{s:?}");
        // Way behind: drop to catch up.
        assert!(!p.step(target + ms_to_frames(SLACK_MS) + 1, 480).put);
    }

    /// A tone encoded with libopus comes out of our decoder as the same tone.
    #[test]
    fn decodes_opus() {
        ffmpeg::init().unwrap();
        let Some(codec) = ffmpeg::encoder::find_by_name("libopus") else {
            eprintln!("skipped: no libopus in this ffmpeg");
            return;
        };
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .unwrap();
        ctx.set_rate(AUDIO_RATE as i32);
        ctx.set_format(ffmpeg::format::Sample::F32(
            ffmpeg::format::sample::Type::Packed,
        ));
        ctx.set_channel_layout(ffmpeg::ChannelLayout::STEREO);
        ctx.set_bit_rate(128_000);
        let mut opts = ffmpeg::Dictionary::new();
        opts.set("frame_duration", "10");
        let mut enc = ctx.open_with(opts).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let mut out = Vec::new();
        let n = AUDIO_FRAMES as usize;
        for k in 0..100 {
            let mut f = ffmpeg::frame::Audio::new(
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                n,
                ffmpeg::ChannelLayout::STEREO,
            );
            f.set_rate(AUDIO_RATE);
            f.set_pts(Some((k * n) as i64));
            let data: &mut [f32] =
                unsafe { std::slice::from_raw_parts_mut(f.data_mut(0).as_mut_ptr().cast(), 2 * n) };
            for i in 0..n {
                let t = (k * n + i) as f32 / AUDIO_RATE as f32;
                data[2 * i] = 0.3 * (std::f32::consts::TAU * 440.0 * t).sin();
                data[2 * i + 1] = 0.0;
            }
            enc.send_frame(&f).unwrap();
            let mut pkt = ffmpeg::Packet::empty();
            while enc.receive_packet(&mut pkt).is_ok() {
                dec.decode(pkt.data().unwrap(), &mut out).unwrap();
            }
        }
        // About a second came out; the left channel is loud, the right quiet.
        assert!(out.len() / 2 > 90 * n, "{} frames", out.len() / 2);
        let rms = |c: usize| {
            let s: Vec<f32> = out
                .iter()
                .skip(c)
                .step_by(2)
                .skip(n * 10)
                .copied()
                .collect();
            (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
        };
        let (l, r) = (rms(0), rms(1));
        assert!(
            (l - 0.212).abs() < 0.03 && r < 0.01,
            "rms left {l}, right {r}"
        );
    }
}
