//! The Mac's sound, for the client: ScreenCaptureKit captures what the Mac
//! plays (minus this process), AudioToolbox encodes it to Opus in packets
//! of 10 ms. Covered by the Screen Recording permission the picture needs
//! anyway; no extra driver.
//!
//! A capture of its own, apart from the picture's: a resize restarts the
//! picture's capture, and the sound shouldn't stop for that.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use aetherdesk_proto::{AUDIO_CHANNELS, AUDIO_FRAMES, AUDIO_RATE, AudioPacket};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty,
    AudioConverterNew, AudioConverterRef, AudioConverterSetProperty, kAudioConverterEncodeBitRate,
    kAudioConverterPropertyMaximumOutputPacketSize,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatOpus,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use crate::capture::{Display, wait};
use crate::stream::micros;

/// Opus at this rate: plenty for music in stereo.
const BITRATE: u32 = 128_000;

/// Where the packets go; called on the capture's queue, so keep it quick.
pub type Sink = Box<dyn Fn(AudioPacket) + Send + Sync>;

/// PCM → Opus packets of [`AUDIO_FRAMES`], with capture times.
struct OpusEncoder {
    converter: AudioConverterRef,
    /// Interleaved stereo waiting to be encoded.
    pending: Vec<f32>,
    /// Capture time (host clock, seconds) of the first frame in `pending`.
    pending_at: f64,
    /// One packet's worth, handed to the converter.
    chunk: Vec<f32>,
    out: Vec<u8>,
    seq: u32,
}

// The converter is only used from one thread at a time (behind a Mutex).
unsafe impl Send for OpusEncoder {}

/// The input callback has nothing more right now (our own code).
const NO_MORE_INPUT: i32 = 0x6e6f6d6f; // 'nomo'

/// What the input callback hands over: the rest of one chunk.
struct Feed {
    data: *const f32,
    frames: u32,
}

unsafe extern "C-unwind" fn feed(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    list: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `user` is the Feed that `encode` passes, alive for the call.
    let feed = unsafe { &mut *user.cast::<Feed>() };
    let wanted = unsafe { packets.as_ptr().read() };
    let take = wanted.min(feed.frames);
    unsafe { packets.as_ptr().write(take) };
    if take == 0 {
        return NO_MORE_INPUT;
    }
    let list = unsafe { &mut *list.as_ptr() };
    list.mNumberBuffers = 1;
    list.mBuffers[0] = AudioBuffer {
        mNumberChannels: AUDIO_CHANNELS,
        mDataByteSize: take * 4 * AUDIO_CHANNELS,
        mData: feed.data as *mut c_void,
    };
    feed.data = unsafe { feed.data.add((take * AUDIO_CHANNELS) as usize) };
    feed.frames -= take;
    0
}

fn pcm_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: f64::from(AUDIO_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * AUDIO_CHANNELS,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * AUDIO_CHANNELS,
        mChannelsPerFrame: AUDIO_CHANNELS,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

impl OpusEncoder {
    fn new() -> Result<Self, String> {
        let input = pcm_format();
        let output = AudioStreamBasicDescription {
            mSampleRate: f64::from(AUDIO_RATE),
            mFormatID: kAudioFormatOpus,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: AUDIO_FRAMES,
            mBytesPerFrame: 0,
            mChannelsPerFrame: AUDIO_CHANNELS,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe {
            AudioConverterNew(
                NonNull::from(&input),
                NonNull::from(&output),
                NonNull::from(&mut converter),
            )
        };
        if status != 0 || converter.is_null() {
            return Err(format!("no Opus encoder (AudioConverterNew: {status})"));
        }
        let bitrate = BITRATE;
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                kAudioConverterEncodeBitRate,
                4,
                NonNull::from(&bitrate).cast(),
            )
        };
        if status != 0 {
            eprintln!("sound: can't set the Opus bitrate ({status}); using the default");
        }
        let mut max_packet: u32 = 0;
        let mut size = 4u32;
        unsafe {
            AudioConverterGetProperty(
                converter,
                kAudioConverterPropertyMaximumOutputPacketSize,
                NonNull::from(&mut size),
                NonNull::from(&mut max_packet).cast(),
            )
        };
        Ok(Self {
            converter,
            pending: Vec::new(),
            pending_at: 0.0,
            chunk: Vec::with_capacity((AUDIO_FRAMES * AUDIO_CHANNELS) as usize),
            out: vec![0; (max_packet as usize).clamp(1500, 64 * 1024)],
            seq: 0,
        })
    }

    /// Adds `pcm` (interleaved stereo, captured at `at`) and passes every
    /// whole packet to `sink`.
    fn push(&mut self, pcm: &[f32], at: f64, sink: &Sink) -> Result<(), String> {
        let frames = self.pending.len() / AUDIO_CHANNELS as usize;
        let expected = self.pending_at + frames as f64 / f64::from(AUDIO_RATE);
        // The first sound, or after a pause (ScreenCaptureKit sends nothing
        // while the Mac is silent): time starts from here.
        if self.pending.is_empty() || (at - expected).abs() > 0.02 {
            self.pending_at = at - frames as f64 / f64::from(AUDIO_RATE);
        }
        self.pending.extend_from_slice(pcm);
        let per_chunk = (AUDIO_FRAMES * AUDIO_CHANNELS) as usize;
        while self.pending.len() >= per_chunk {
            self.chunk.clear();
            self.chunk.extend(self.pending.drain(..per_chunk));
            let captured_us = micros(self.pending_at);
            self.pending_at += f64::from(AUDIO_FRAMES) / f64::from(AUDIO_RATE);
            self.encode_chunk(captured_us, sink)?;
        }
        Ok(())
    }

    fn encode_chunk(&mut self, captured_us: u64, sink: &Sink) -> Result<(), String> {
        let mut input = Feed {
            data: self.chunk.as_ptr(),
            frames: AUDIO_FRAMES,
        };
        loop {
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: AUDIO_CHANNELS,
                    mDataByteSize: self.out.len() as u32,
                    mData: self.out.as_mut_ptr().cast(),
                }],
            };
            let mut packets = 1u32;
            let mut description = AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            };
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    Some(feed),
                    (&mut input as *mut Feed).cast(),
                    NonNull::from(&mut packets),
                    NonNull::from(&mut list),
                    &mut description,
                )
            };
            if packets > 0 {
                let len = match description.mDataByteSize {
                    0 => list.mBuffers[0].mDataByteSize,
                    n => n,
                } as usize;
                sink(AudioPacket {
                    seq: self.seq,
                    captured_us,
                    data: self.out[..len.min(self.out.len())].to_vec(),
                });
                self.seq = self.seq.wrapping_add(1);
            }
            match status {
                0 if packets > 0 => continue, // maybe more
                0 | NO_MORE_INPUT => return Ok(()),
                e => return Err(format!("encoding sound: {e}")),
            }
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

/// Room for a two-buffer list (planar stereo), which the one-buffer
/// `AudioBufferList` type can't hold.
#[repr(C)]
struct StereoBufferList {
    count: u32,
    buffers: [AudioBuffer; 2],
}

/// The sound in `sample` as interleaved stereo `f32`. ScreenCaptureKit
/// delivers 32-bit float, usually one buffer per channel.
///
/// # Safety
/// `sample` must be an audio sample buffer from ScreenCaptureKit.
unsafe fn interleaved(sample: &CMSampleBuffer, out: &mut Vec<f32>) -> Result<(), String> {
    let format = unsafe { sample.format_description() }.ok_or("sound without a format")?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref() }
        .ok_or("sound format isn't PCM")?;
    if asbd.mFormatID != kAudioFormatLinearPCM
        || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0
        || asbd.mBitsPerChannel != 32
    {
        return Err(format!(
            "unexpected sound format: id {:#x}, flags {:#x}, {} bit",
            asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel
        ));
    }
    if asbd.mSampleRate as u32 != AUDIO_RATE {
        return Err(format!(
            "sound at {} Hz, not {AUDIO_RATE}",
            asbd.mSampleRate
        ));
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let mut list = StereoBufferList {
        count: 0,
        buffers: [AudioBuffer {
            mNumberChannels: 0,
            mDataByteSize: 0,
            mData: std::ptr::null_mut(),
        }; 2],
    };
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            (&mut list as *mut StereoBufferList).cast(),
            std::mem::size_of::<StereoBufferList>(),
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        )
    };
    // Keeps the sound's memory alive until we've copied it.
    let _block = NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    if status != 0 {
        return Err(format!("reading sound: {status}"));
    }
    let slice = |b: &AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(b.mData.cast(), b.mDataByteSize as usize / 4) }
    };
    out.clear();
    match (planar, list.count) {
        (true, 2..) => {
            let (l, r) = (slice(&list.buffers[0]), slice(&list.buffers[1]));
            out.extend(l.iter().zip(r).flat_map(|(&l, &r)| [l, r]));
        }
        // Mono, one way or the other: the same on both sides.
        (true, 1) | (false, 1) if channels == 1 => {
            out.extend(slice(&list.buffers[0]).iter().flat_map(|&v| [v, v]));
        }
        (false, 1) => {
            // Interleaved: keep the first two channels.
            let s = slice(&list.buffers[0]);
            out.extend(
                s.chunks_exact(channels)
                    .flat_map(|f| [f[0], f[1.min(channels - 1)]]),
            );
        }
        (_, n) => return Err(format!("{n} sound buffers for {channels} channel(s)")),
    }
    Ok(())
}

struct OutputIvars {
    encoder: Mutex<OpusEncoder>,
    sink: Sink,
    pcm: Mutex<Vec<f32>>,
    packets: AtomicU64,
    /// Errors so far; only the first few are printed.
    errors: AtomicU64,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; AudioOutput has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "AetherDeskAudioOutput"]
    #[ivars = OutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return; // the stream's (tiny) picture: not ours to use
            }
            let ivars = self.ivars();
            let mut pcm = ivars.pcm.lock().unwrap();
            let at = unsafe { sample.presentation_time_stamp().seconds() };
            let result = unsafe { interleaved(sample, &mut pcm) }
                .and_then(|()| ivars.encoder.lock().unwrap().push(&pcm, at, &ivars.sink));
            match result {
                Ok(()) => {
                    ivars.packets.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    if ivars.errors.fetch_add(1, Ordering::Relaxed) < 3 {
                        eprintln!("sound: {e}");
                    }
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            eprintln!(
                "sound capture stopped: {} (code {})",
                error.localizedDescription(),
                error.code()
            );
        }
    }
);

impl AudioOutput {
    fn new(encoder: OpusEncoder, sink: Sink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            encoder: Mutex::new(encoder),
            sink,
            pcm: Mutex::new(Vec::new()),
            packets: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// A running sound capture.
pub struct AudioCapture {
    stream: Retained<SCStream>,
    output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl AudioCapture {
    /// Captures the sound of `display`'s session and hands every Opus
    /// packet to `sink`.
    pub fn start(display: &Display, sink: Sink) -> Result<Self, String> {
        let encoder = OpusEncoder::new()?;
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display.display,
                &NSArray::<SCWindow>::new(),
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setCapturesAudio(true);
            config.setSampleRate(AUDIO_RATE as isize);
            config.setChannelCount(AUDIO_CHANNELS as isize);
            config.setExcludesCurrentProcessAudio(true);
            // A stream always has a picture; keep this one tiny and rare.
            config.setWidth(16);
            config.setHeight(16);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setShowsCursor(false);
        }
        let output = AudioOutput::new(encoder, sink);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        let queue = DispatchQueue::new("nl.stormlantern.aetherdesk.audio", None);
        // The picture output too, though unused: without one,
        // ScreenCaptureKit logs every frame it has to drop.
        for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
            unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    kind,
                    Some(&queue),
                )
            }
            .map_err(|e| format!("adding sound output: {}", e.localizedDescription()))?;
        }
        wait(|block| unsafe { stream.startCaptureWithCompletionHandler(Some(block)) })
            .map_err(|e| format!("starting sound capture: {e}"))?;
        Ok(Self {
            stream,
            output,
            _queue: queue,
        })
    }

    /// Stops capturing; says how many sound buffers came in.
    pub fn stop(self) {
        if let Err(e) =
            wait(|block| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(block)) })
        {
            eprintln!("stopping sound capture: {e}");
        }
        let ivars = self.output.ivars();
        println!(
            "sound: stopped after {} buffers ({} errors), {} packets sent",
            ivars.packets.load(Ordering::Relaxed),
            ivars.errors.load(Ordering::Relaxed),
            ivars.encoder.lock().unwrap().seq
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second of a 440 Hz tone, fed in uneven pieces like
    /// ScreenCaptureKit's, comes out as 100 packets with times 10 ms apart.
    #[test]
    fn encodes_opus_packets_every_10_ms() {
        let mut encoder = OpusEncoder::new().unwrap();
        let packets = std::sync::Arc::new(Mutex::new(Vec::new()));
        let collect = packets.clone();
        let sink: Sink = Box::new(move |p| collect.lock().unwrap().push(p));
        let tone: Vec<f32> = (0..AUDIO_RATE as usize)
            .flat_map(|i| {
                let v = 0.3 * (std::f32::consts::TAU * 440.0 * i as f32 / AUDIO_RATE as f32).sin();
                [v, v]
            })
            .collect();
        let start = 1000.0;
        let mut at = 0;
        for piece in tone.chunks(2 * 1024) {
            let t = start + (at / 2) as f64 / f64::from(AUDIO_RATE);
            encoder.push(piece, t, &sink).unwrap();
            at += piece.len();
        }
        let packets = packets.lock().unwrap();
        assert!(
            (98..=100).contains(&packets.len()),
            "{} packets",
            packets.len()
        );
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.seq, i as u32);
            assert!(
                !p.data.is_empty() && p.data.len() < 1000,
                "{}",
                p.data.len()
            );
        }
        let gaps: Vec<u64> = packets
            .windows(2)
            .map(|w| w[1].captured_us - w[0].captured_us)
            .collect();
        assert!(
            gaps.iter().all(|&g| (9_990..=10_010).contains(&g)),
            "{gaps:?}"
        );
        assert_eq!(packets[0].captured_us, 1_000_000_000);
    }
}
