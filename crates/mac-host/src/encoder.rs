//! H.264 encoding with VideoToolbox, tuned for low latency: real-time mode,
//! no B-frames, low-latency rate control, keyframes mostly on request.
//!
//! At full Retina size the encoder can't always keep up with 60 fps. Frames
//! it can't take right away would queue up in front of it and every frame
//! would arrive late (measured: ~140 ms). So at most [`Settings::max_in_flight`] frames
//! are in the encoder at once and newer captures are skipped until one comes
//! out: a lower frame rate, but each frame is fresh.

use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};
use objc2_core_media::{
    CMClock, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use crate::annexb;

/// One encoded frame, Annex B, with SPS/PPS in front of every keyframe.
#[derive(Debug)]
pub struct EncodedFrame {
    /// Presentation time in seconds on the host clock (from ScreenCaptureKit).
    pub pts: f64,
    /// When the encoder handed the frame back, in seconds on the same clock.
    pub encoded_at: f64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub type EncoderOutput = Sender<Result<EncodedFrame, String>>;

pub struct Settings {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Frames the encoder may hold at once; more and they'd queue up.
    /// Fewer means fresher frames but maybe a lower frame rate.
    pub max_in_flight: usize,
}

/// Default for [`Settings::max_in_flight`]. Measured on an M3 at 3456x2234:
/// one frame gave ~29 ms encode at ~44 fps, two gave ~45 ms at ~52 fps.
/// Fresher frames felt better than the extra frame rate.
pub const DEFAULT_IN_FLIGHT: usize = 1;

pub struct Encoder {
    shared: Arc<Shared>,
}

/// What the capture queue and the encoder's own queue both touch.
struct Shared {
    session: CFRetained<VTCompressionSession>,
    output: EncoderOutput,
    force_keyframe: Arc<AtomicBool>,
    skipped: Arc<AtomicU64>,
    max_in_flight: usize,
    gate: Mutex<Gate>,
    /// Where finished frames free their slot and pending frames go in.
    queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: a VTCompressionSession and the CVImageBuffers it encodes are
// thread-safe Core Foundation objects; every submission happens under `gate`.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

#[derive(Default)]
struct Gate {
    in_flight: usize,
    /// The newest frame that arrived while the encoder was full. It goes in
    /// as soon as a slot frees, so the last picture of a motion is never
    /// lost when the screen then stands still.
    pending: Option<(CFRetained<CVImageBuffer>, CMTime)>,
    stopped: bool,
}

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed: OSStatus {status}"))
    }
}

fn yes() -> &'static CFType {
    CFBoolean::new(true)
}

fn no() -> &'static CFType {
    CFBoolean::new(false)
}

impl Encoder {
    pub fn new(s: &Settings, output: EncoderOutput) -> Result<Self, String> {
        let spec = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }],
            &[yes()],
        );
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                s.width as i32,
                s.height as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                None,
                None,
                None,
                ptr::null_mut(),
                NonNull::from(&mut raw),
            )
        };
        check(status, "VTCompressionSessionCreate")?;
        let session = unsafe { CFRetained::from_raw(NonNull::new(raw).ok_or("no session")?) };

        let set = |key: &CFString, value: &CFType, name: &str| {
            check(
                unsafe { VTSessionSetProperty(&session, key, Some(value)) },
                name,
            )
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, yes(), "RealTime")?;
            set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                no(),
                "AllowFrameReordering",
            )?;
            set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
                "ProfileLevel",
            )?;
            set(
                kVTCompressionPropertyKey_AverageBitRate,
                &CFNumber::new_i32(s.bitrate_bps as i32),
                "AverageBitRate",
            )?;
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(s.fps as i32),
                "ExpectedFrameRate",
            )?;
            // Keyframes come on request (new client, decoder lost sync); this is only a safety net.
            set(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &CFNumber::new_f64(10.0),
                "MaxKeyFrameIntervalDuration",
            )?;
            // Speed hints. Not every encoder supports them, and they only
            // help, so a refusal isn't an error.
            let optional = |key: &CFString, value: &CFType, name: &str| {
                if let Err(e) = set(key, value, name) {
                    eprintln!("note: encoder ignores {name} ({e})");
                }
            };
            optional(
                kVTCompressionPropertyKey_MaxFrameDelayCount,
                &CFNumber::new_i32(0),
                "MaxFrameDelayCount",
            );
            optional(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                yes(),
                "PrioritizeEncodingSpeedOverQuality",
            );
            check(
                session.prepare_to_encode_frames(),
                "VTCompressionSessionPrepareToEncodeFrames",
            )?;
        }

        Ok(Self {
            shared: Arc::new(Shared {
                session,
                output,
                force_keyframe: Arc::new(AtomicBool::new(false)),
                skipped: Arc::new(AtomicU64::new(0)),
                max_in_flight: s.max_in_flight.max(1),
                gate: Mutex::new(Gate::default()),
                queue: DispatchQueue::new("nl.stormlantern.aetherdesk.encoder", None),
            }),
        })
    }

    /// Counts captured frames skipped because the encoder was busy.
    pub fn skipped_counter(&self) -> Arc<AtomicU64> {
        self.shared.skipped.clone()
    }

    /// Handle to request a keyframe from another thread.
    pub fn keyframe_trigger(&self) -> Arc<AtomicBool> {
        self.shared.force_keyframe.clone()
    }

    /// Offers one frame. The result arrives on the output channel, possibly
    /// on another thread; a frame the encoder drops produces nothing. While
    /// the encoder holds `max_in_flight` frames, the newest offer waits
    /// for a free slot and older waiting ones are skipped.
    pub fn encode(&self, image: &CVImageBuffer, pts: CMTime) {
        let mut gate = self.shared.gate.lock().unwrap();
        if gate.stopped {
            return;
        }
        if gate.in_flight >= self.shared.max_in_flight {
            if gate.pending.replace((image.retain(), pts)).is_some() {
                self.shared.skipped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        // A newer frame supersedes one still waiting.
        if gate.pending.take().is_some() {
            self.shared.skipped.fetch_add(1, Ordering::Relaxed);
        }
        Shared::submit(&self.shared, &mut gate, image, pts);
    }

    /// Flushes frames still in the encoder.
    pub fn finish(&self) {
        {
            let mut gate = self.shared.gate.lock().unwrap();
            gate.stopped = true;
            gate.pending = None;
        }
        unsafe { self.shared.session.complete_frames(kCMTimeInvalid) };
    }
}

impl Shared {
    /// Hands a frame to VideoToolbox. Called with the gate held, so frames
    /// go in in capture order.
    fn submit(this: &Arc<Self>, gate: &mut Gate, image: &CVImageBuffer, pts: CMTime) {
        gate.in_flight += 1;
        let props = this.force_keyframe.swap(false, Ordering::Relaxed).then(|| {
            CFDictionary::<CFString, CFType>::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[yes()],
            )
        });
        let shared = this.clone();
        let handler = RcBlock::new(
            move |status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                let result = if status != 0 {
                    Some(Err(format!("encoding a frame failed: OSStatus {status}")))
                } else {
                    // None: dropped by the encoder.
                    unsafe { sample.as_ref() }.map(|s| unsafe { to_annexb(s) })
                };
                if let Some(result) = result {
                    let _ = shared.output.send(result);
                }
                // Free the slot on our own queue: VideoToolbox may call this
                // from inside `submit`, where the gate is still held.
                let shared = shared.clone();
                shared
                    .queue
                    .clone()
                    .exec_async(move || Shared::done(&shared));
            },
        );
        let status = unsafe {
            this.session.encode_frame_with_output_handler(
                image,
                pts,
                kCMTimeInvalid,
                props.as_deref().map(|d| d.as_opaque()),
                ptr::null_mut(),
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            // The handler won't run for a frame that wasn't queued.
            gate.in_flight -= 1;
            let _ = this
                .output
                .send(Err(format!("queueing a frame failed: OSStatus {status}")));
        }
    }

    /// A frame came out: free its slot and send in the waiting frame, if any.
    fn done(this: &Arc<Self>) {
        let mut gate = this.gate.lock().unwrap();
        gate.in_flight = gate.in_flight.saturating_sub(1);
        if gate.stopped {
            return;
        }
        if let Some((image, pts)) = gate.pending.take() {
            Shared::submit(this, &mut gate, &image, pts);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Here, not on Shared: its last reference may go away inside a
        // VideoToolbox callback, which must not tear down the session.
        self.shared.gate.lock().unwrap().stopped = true;
        unsafe { self.shared.session.invalidate() };
    }
}

/// Now, in seconds on the host time clock: the clock ScreenCaptureKit
/// stamps frames with (`mach_absolute_time`).
pub fn host_time() -> f64 {
    unsafe { CMClock::host_time_clock().time().seconds() }
}

/// # Safety
/// `sample` must be an encoded H.264 sample buffer from VideoToolbox.
unsafe fn to_annexb(sample: &CMSampleBuffer) -> Result<EncodedFrame, String> {
    let keyframe = unsafe { is_keyframe(sample) };
    let block = unsafe { sample.data_buffer() }.ok_or("sample has no data")?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if len > 0 {
        let dst = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()).expect("non-empty vec");
        check(
            unsafe { block.copy_data_bytes(0, len, dst) },
            "CMBlockBufferCopyDataBytes",
        )?;
    }
    let format = unsafe { sample.format_description() }.ok_or("sample has no format")?;

    let mut out = Vec::with_capacity(len + 64);
    let (count, prefix_size) = unsafe { parameter_set_info(&format) }?;
    if keyframe {
        for i in 0..count {
            annexb::push_parameter_set(&mut out, unsafe { parameter_set(&format, i) }?);
        }
    }
    annexb::push_avcc(&mut out, &avcc, prefix_size).map_err(|e| e.to_string())?;

    let pts = unsafe { sample.presentation_time_stamp().seconds() };
    Ok(EncodedFrame {
        pts,
        encoded_at: host_time(),
        keyframe,
        data: out,
    })
}

unsafe fn parameter_set_info(format: &CMFormatDescription) -> Result<(usize, usize), String> {
    let mut count = 0usize;
    let mut prefix: c_int = 0;
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut prefix,
        )
    };
    check(status, "CMVideoFormatDescriptionGetH264ParameterSetAtIndex")?;
    Ok((count, prefix as usize))
}

unsafe fn parameter_set(format: &CMFormatDescription, index: usize) -> Result<&[u8], String> {
    let mut data: *const u8 = ptr::null();
    let mut size = 0usize;
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            index,
            &mut data,
            &mut size,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    check(status, "CMVideoFormatDescriptionGetH264ParameterSetAtIndex")?;
    if data.is_null() {
        return Err(format!("parameter set {index} is missing"));
    }
    // The format description owns this memory and outlives the borrow.
    Ok(unsafe { std::slice::from_raw_parts(data, size) })
}

/// A sample is a keyframe unless its attachments say `NotSync`.
unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    if attachments.count() == 0 {
        return true;
    }
    let dict = unsafe { attachments.value_at_index(0) }.cast::<CFDictionary>();
    let Some(dict) = (unsafe { dict.as_ref() }) else {
        return true;
    };
    let key: *const CFString = unsafe { kCMSampleAttachmentKey_NotSync };
    let value = unsafe { dict.value(key.cast()) }.cast::<CFBoolean>();
    match unsafe { value.as_ref() } {
        Some(not_sync) => !not_sync.value(),
        None => true,
    }
}
