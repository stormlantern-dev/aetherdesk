//! The microphone, for the Mac (#82): SDL records the default input
//! (through PipeWire), ffmpeg's libopus encodes it in packets of 10 ms, and
//! the network thread sends them as datagrams. The host plays them as the
//! Auroval device's input, so a call on the Mac hears this microphone.
//!
//! Only with `--mic`, and only when the host said it takes it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use auroval_proto::{AUDIO_FRAMES, AUDIO_RATE, AudioPacket, MIC_CHANNELS};
use ffmpeg_next as ffmpeg;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};
use sdl3::sys::audio as sys;
use tokio::sync::mpsc::UnboundedSender;

use crate::latency::now_us;

/// Speech, not music: plenty for a voice in mono.
const BITRATE: usize = 48_000;

/// libopus, set up for 10 ms packets of mono speech.
pub struct OpusEncoder {
    inner: ffmpeg::encoder::Audio,
    frame: ffmpeg::frame::Audio,
    packet: ffmpeg::Packet,
    pts: i64,
}

impl OpusEncoder {
    pub fn new() -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let codec = ffmpeg::encoder::find_by_name("libopus")
            .context("this ffmpeg has no libopus, which the microphone needs")?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()?;
        ctx.set_rate(AUDIO_RATE as i32);
        ctx.set_format(ffmpeg::format::Sample::F32(
            ffmpeg::format::sample::Type::Packed,
        ));
        ctx.set_channel_layout(ffmpeg::ChannelLayout::MONO);
        ctx.set_bit_rate(BITRATE);
        let mut opts = ffmpeg::Dictionary::new();
        opts.set("frame_duration", "10");
        opts.set("application", "voip");
        let inner = ctx.open_with(opts).context("opening libopus")?;
        let frames = AUDIO_FRAMES as usize;
        let mut frame = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            frames,
            ffmpeg::ChannelLayout::MONO,
        );
        frame.set_rate(AUDIO_RATE);
        Ok(Self {
            inner,
            frame,
            packet: ffmpeg::Packet::empty(),
            pts: 0,
        })
    }

    /// Encodes one packet's worth ([`AUDIO_FRAMES`] samples) and passes
    /// every packet that comes out to `out`.
    pub fn encode(&mut self, pcm: &[f32], mut out: impl FnMut(&[u8])) -> anyhow::Result<()> {
        let n = AUDIO_FRAMES as usize;
        if pcm.len() != n {
            bail!("{} samples, expected {n}", pcm.len());
        }
        let data: &mut [f32] = unsafe {
            std::slice::from_raw_parts_mut(self.frame.data_mut(0).as_mut_ptr().cast(), n)
        };
        data.copy_from_slice(pcm);
        self.frame.set_pts(Some(self.pts));
        self.pts += n as i64;
        self.inner.send_frame(&self.frame)?;
        while self.inner.receive_packet(&mut self.packet).is_ok() {
            if let Some(bytes) = self.packet.data() {
                out(bytes);
            }
        }
        Ok(())
    }
}

/// An SDL stream pointer for the recording thread (see audio::RawStream).
struct RawStream(*mut sys::SDL_AudioStream);
unsafe impl Send for RawStream {}

/// Records the microphone while it lives.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    sent: Arc<AtomicU64>,
    // Last: dropped after the thread is joined (see Drop).
    _stream: AudioStreamOwner,
}

impl Recorder {
    pub fn start(sdl: &sdl3::Sdl, out: UnboundedSender<AudioPacket>) -> anyhow::Result<Self> {
        let audio = sdl.audio().map_err(|e| anyhow!("SDL audio: {e}"))?;
        let spec = AudioSpec {
            freq: Some(AUDIO_RATE as i32),
            channels: Some(MIC_CHANNELS as i32),
            format: Some(AudioFormat::f32_sys()),
        };
        let stream = audio
            .default_recording_device()
            .open_device_stream(Some(&spec))
            .map_err(|e| anyhow!("opening the microphone: {e}"))?;
        let mut encoder = OpusEncoder::new()?;
        let raw = RawStream(stream.raw());
        if !unsafe { sys::SDL_ResumeAudioStreamDevice(raw.0) } {
            bail!("starting the microphone: {}", sdl3::get_error());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (stop2, sent2) = (stop.clone(), sent.clone());
        say!("microphone: sending to the Mac (Auroval microphone)");
        let thread = thread::Builder::new().name("mic".into()).spawn(move || {
            let raw = raw;
            if let Err(e) = record(raw.0, &mut encoder, &out, &stop2, &sent2) {
                warn!("microphone stopped: {e:#}");
            }
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
            sent,
            _stream: stream,
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        say!(
            "microphone: {} packets sent",
            self.sent.load(Ordering::Relaxed)
        );
    }
}

/// The recording thread: takes what SDL recorded every few ms, encodes it
/// in packets of 10 ms and hands them to the network thread.
fn record(
    stream: *mut sys::SDL_AudioStream,
    encoder: &mut OpusEncoder,
    out: &UnboundedSender<AudioPacket>,
    stop: &AtomicBool,
    sent: &AtomicU64,
) -> anyhow::Result<()> {
    let n = AUDIO_FRAMES as usize * MIC_CHANNELS as usize;
    let mut pending: Vec<f32> = Vec::with_capacity(4 * n);
    let mut buf = vec![0f32; 4 * n];
    let mut seq = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let got = unsafe {
            sys::SDL_GetAudioStreamData(
                stream,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(buf.as_slice()) as i32,
            )
        };
        if got < 0 {
            bail!("reading the microphone: {}", sdl3::get_error());
        }
        if got == 0 {
            thread::sleep(Duration::from_millis(3));
            continue;
        }
        pending.extend_from_slice(&buf[..got as usize / 4]);
        while pending.len() >= n {
            // When the oldest sample in the packet was recorded, about.
            let captured_us = now_us() - (pending.len() as i64 * 1_000_000 / AUDIO_RATE as i64);
            let chunk: Vec<f32> = pending.drain(..n).collect();
            let mut closed = false;
            encoder.encode(&chunk, |data| {
                let packet = AudioPacket {
                    seq,
                    captured_us: captured_us.max(0) as u64,
                    data: data.to_vec(),
                };
                seq = seq.wrapping_add(1);
                closed |= out.send(packet).is_err();
                sent.fetch_add(1, Ordering::Relaxed);
            })?;
            if closed {
                return Ok(()); // the link is gone
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono tone through our encoder comes out as Opus packets of 10 ms
    /// that our own player's decoder (stereo) can read.
    #[test]
    fn encodes_mono_opus_packets() {
        let Ok(mut enc) = OpusEncoder::new() else {
            eprintln!("skipped: no libopus in this ffmpeg");
            return;
        };
        let n = AUDIO_FRAMES as usize;
        let mut packets = Vec::new();
        for k in 0..50 {
            let pcm: Vec<f32> = (0..n)
                .map(|i| {
                    let t = (k * n + i) as f32 / AUDIO_RATE as f32;
                    0.3 * (std::f32::consts::TAU * 440.0 * t).sin()
                })
                .collect();
            enc.encode(&pcm, |p| packets.push(p.to_vec())).unwrap();
        }
        assert!((45..=50).contains(&packets.len()), "{}", packets.len());
        let mut dec = crate::audio::OpusDecoder::new().unwrap();
        let mut out = Vec::new();
        for p in &packets {
            dec.decode(p, &mut out).unwrap();
        }
        // About 10 ms per packet, whatever the channel count.
        assert!(out.len() / 2 >= (packets.len() - 2) * n, "{}", out.len());
        let tail = &out[out.len() / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 0.1, "rms {rms}");
    }
}
