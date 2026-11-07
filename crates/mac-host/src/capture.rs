//! Continuous capture of the main display with ScreenCaptureKit. Frames arrive
//! as NV12 pixel buffers on a private dispatch queue and go straight into the
//! encoder there, so pixel data never crosses into Rust threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVImageBuffer, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::dump::Dump;
use crate::encoder::{Encoder, EncoderOutput};
use crate::input::Rect;
use crate::scale::Scaler;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Lets a value cross from a completion-handler queue to the waiting thread.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

fn ns_error(err: *mut NSError) -> String {
    match unsafe { err.as_ref() } {
        Some(e) => format!("{} (code {})", e.localizedDescription(), e.code()),
        None => "unknown error".into(),
    }
}

pub struct Display {
    pub display: Retained<SCDisplay>,
    /// Size in pixels (Retina: twice the size in points).
    pub width: usize,
    pub height: usize,
    /// Position and size in global display coordinates (points), which is
    /// what mouse events use.
    pub bounds: Rect,
}

/// Processes whose windows every capture leaves out: our own on-screen
/// indicator ([`crate::indicator`]).
static EXCLUDED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Leaves `pid`'s windows out of captures started from now on.
pub fn exclude_process(pid: i32) {
    EXCLUDED.lock().unwrap().push(pid);
}

/// Undoes [`exclude_process`].
pub fn include_process(pid: i32) {
    EXCLUDED.lock().unwrap().retain(|p| *p != pid);
}

/// What can be captured right now: displays, windows and their apps.
fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let res = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(ns_error(err)),
        };
        let _ = tx.send(res);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(format!("listing displays: {e}")),
        Err(_) => Err("listing displays timed out".into()),
    }
}

/// The main display, with its size in pixels.
pub fn main_display() -> Result<Display, String> {
    let content = shareable_content()?;
    let display = unsafe { content.displays() }
        .firstObject()
        .ok_or("no displays")?;
    let b = CGDisplayBounds(unsafe { display.displayID() });
    let bounds = Rect {
        x: b.origin.x,
        y: b.origin.y,
        width: b.size.width,
        height: b.size.height,
    };
    let mode = CGDisplayCopyDisplayMode(unsafe { display.displayID() })
        .ok_or("no display mode for the main display")?;
    let width = CGDisplayMode::pixel_width(Some(&mode));
    let height = CGDisplayMode::pixel_height(Some(&mode));
    Ok(Display {
        display,
        width,
        height,
        bounds,
    })
}

struct OutputIvars {
    encoder: Encoder,
    errors: EncoderOutput,
    /// Our own downscale (`AUROVAL_CAPTURE_SCALE`), and whether it failed.
    scaler: Option<(Scaler, AtomicBool)>,
    /// `AUROVAL_DUMP_FRAME`.
    dump: Option<Dump>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; StreamOutput has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "AurovalStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Frames without an image are "nothing changed" ticks.
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                self.ivars().encoder.idle();
                return;
            };
            let pts = unsafe { sample.presentation_time_stamp() };
            let image = self.prepare(image);
            self.ivars().encoder.encode(&image, pts);
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!(
                "capture stopped: {} (code {})",
                error.localizedDescription(),
                error.code()
            );
            let _ = self.ivars().errors.send(Err(msg));
        }
    }
);

impl StreamOutput {
    fn new(encoder: Encoder, errors: EncoderOutput, scaler: Option<Scaler>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            encoder,
            errors,
            scaler: scaler.map(|s| (s, AtomicBool::new(false))),
            dump: Dump::from_env(),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// The #113 experiments, when switched on: our own downscale, saving a
    /// frame. Otherwise `image` as it is.
    fn prepare(&self, image: CFRetained<CVImageBuffer>) -> CFRetained<CVImageBuffer> {
        let ivars = self.ivars();
        if let Some(d) = &ivars.dump {
            d.offer(&image);
        }
        let Some((scaler, failed)) = &ivars.scaler else {
            return image;
        };
        match scaler.scale(&image) {
            Ok(scaled) => {
                if let Some(d) = &ivars.dump {
                    d.offer(&scaled);
                }
                scaled
            }
            // The encoder scales a bigger frame itself.
            Err(e) => {
                if !failed.swap(true, Ordering::Relaxed) {
                    eprintln!("capture: {e}; the encoder scales instead");
                }
                image
            }
        }
    }
}

/// The whole display, minus the processes in [`EXCLUDED`]. Leaving out an
/// application covers the windows it opens later too.
fn filter(display: &Display) -> Result<Retained<SCContentFilter>, String> {
    let excluded = EXCLUDED.lock().unwrap().clone();
    if excluded.is_empty() {
        return Ok(unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display.display,
                &NSArray::<SCWindow>::new(),
            )
        });
    }
    let content = shareable_content()?;
    let apps: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
        .iter()
        .filter(|a| excluded.contains(&unsafe { a.processID() }))
        .collect();
    if apps.len() < excluded.len() {
        println!(
            "capture: {} of {} processes to leave out not found",
            excluded.len() - apps.len(),
            excluded.len()
        );
    }
    Ok(unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display.display,
            &NSArray::from_retained_slice(&apps),
            &NSArray::<SCWindow>::new(),
        )
    })
}

pub struct Capture {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl Capture {
    /// Starts capturing `display` at `fps`, feeding every frame to `encoder`.
    /// Encoded frames and errors go to `output`. Without `shows_cursor` the
    /// pointer is left out of the picture (the client draws it). With
    /// `full_color` the capture is RGB (for 4:4:4), else 4:2:0.
    pub fn start(
        display: &Display,
        size: (usize, usize),
        fps: u32,
        encoder: Encoder,
        output: EncoderOutput,
        shows_cursor: bool,
        full_color: bool,
    ) -> Result<Self, String> {
        let filter = filter(display)?;
        // 4:4:4 needs every pixel's colour: capture RGB and let the
        // encoder convert. Otherwise 4:2:0 straight from the capture.
        let format = if full_color {
            kCVPixelFormatType_32BGRA
        } else {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        };
        let (captured, scaler) = scaled_capture(display, size, format)?;
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            // ScreenCaptureKit scales to this on the GPU.
            config.setWidth(captured.0);
            config.setHeight(captured.1);
            config.setPixelFormat(format);
            config.setMinimumFrameInterval(CMTime::new(frame_interval_us(fps), 1_000_000));
            config.setQueueDepth(5);
            config.setShowsCursor(shows_cursor);
        }

        let handler = StreamOutput::new(encoder, output, scaler);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*handler)),
            )
        };
        let queue = DispatchQueue::new("nl.stormlantern.auroval.capture", None);
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*handler),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| format!("adding stream output: {}", e.localizedDescription()))?;

        wait(|block| unsafe { stream.startCaptureWithCompletionHandler(Some(block)) })
            .map_err(|e| format!("starting capture: {e}"))?;
        Ok(Self {
            stream,
            output: handler,
            _queue: queue,
        })
    }

    /// Stops capturing and flushes the encoder.
    pub fn stop(self) -> Result<(), String> {
        let res =
            wait(|block| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(block)) })
                .map_err(|e| format!("stopping capture: {e}"));
        self.output.ivars().encoder.finish();
        res
    }
}

/// What to capture at for a `size` picture, and our scaler when that's
/// bigger (`AUROVAL_CAPTURE_SCALE`, #113).
fn scaled_capture(
    display: &Display,
    size: (usize, usize),
    format: u32,
) -> Result<((usize, usize), Option<Scaler>), String> {
    let tuning = crate::tuning::Tuning::get();
    let (w, h) = crate::tuning::capture_size(
        (display.width as u32, display.height as u32),
        (size.0 as u32, size.1 as u32),
        tuning.capture_scale,
    );
    let captured = (w as usize, h as usize);
    if captured == size {
        return Ok((size, None));
    }
    let scaler = Scaler::new(size, format, tuning.downsample)?;
    println!(
        "capture: {}x{}, scaled to {}x{} ({:?})",
        captured.0, captured.1, size.0, size.1, tuning.downsample
    );
    Ok((captured, Some(scaler)))
}

/// Set to a number of microseconds, the shortest time between two captures,
/// overriding [`default_interval_us`]; 0 captures every refresh.
pub const INTERVAL_ENV: &str = "AUROVAL_CAPTURE_INTERVAL_US";

/// The shortest time between two captures at `fps`: 90 % of a frame, not a
/// whole one. The built-in display refreshes at 120 Hz, and with exactly 1/60 s
/// a tick that comes a hair early is passed over, leaving a 25 ms gap (#109).
/// At 90 % every second 120 Hz tick still qualifies, so a 60 Hz stream gets
/// one capture per frame.
fn default_interval_us(fps: u32) -> i64 {
    1_000_000 * 9 / (10 * i64::from(fps.max(1)))
}

/// The minimum frame interval for `fps`, in microseconds; see [`INTERVAL_ENV`].
fn frame_interval_us(fps: u32) -> i64 {
    let default = default_interval_us(fps);
    match std::env::var(INTERVAL_ENV)
        .ok()
        .map(|v| v.trim().parse::<i64>())
    {
        None => default,
        Some(Ok(us)) if us >= 0 => {
            println!("capture: at least {us} µs between frames ({INTERVAL_ENV})");
            us
        }
        Some(_) => {
            eprintln!("{INTERVAL_ENV}: not a number of microseconds; using {default}");
            default
        }
    }
}

/// Calls `start` with a completion block and waits for it.
pub(crate) fn wait(
    start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(if err.is_null() {
            Ok(())
        } else {
            Err(ns_error(err))
        });
    });
    start(&block);
    rx.recv_timeout(TIMEOUT)
        .map_err(|_| "timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::default_interval_us;

    #[test]
    fn default_interval_leaves_room_for_an_early_tick() {
        // 60 fps on a 120 Hz display: 15 ms, so every second tick qualifies.
        assert_eq!(default_interval_us(60), 15_000);
        assert!(default_interval_us(60) < 2 * 1_000_000 / 120);
        assert_eq!(default_interval_us(30), 30_000);
        assert_eq!(default_interval_us(0), 900_000);
    }
}
