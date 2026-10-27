//! Continuous capture of the main display with ScreenCaptureKit. Frames arrive
//! as NV12 pixel buffers on a private dispatch queue and go straight into the
//! encoder there, so pixel data never crosses into Rust threads.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_graphics::{CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::encoder::{Encoder, EncoderOutput};
use crate::input::Rect;

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

/// The main display, with its size in pixels.
pub fn main_display() -> Result<Display, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let res = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(ns_error(err)),
        };
        let _ = tx.send(res);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let content = match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(c)) => c.0,
        Ok(Err(e)) => return Err(format!("listing displays: {e}")),
        Err(_) => return Err("listing displays timed out".into()),
    };
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
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; StreamOutput has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "AetherDeskStreamOutput"]
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
                return;
            };
            let pts = unsafe { sample.presentation_time_stamp() };
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
    fn new(encoder: Encoder, errors: EncoderOutput) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { encoder, errors });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Capture {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl Capture {
    /// Starts capturing `display` at `fps`, feeding every frame to `encoder`.
    /// Encoded frames and errors go to `output`.
    pub fn start(
        display: &Display,
        fps: u32,
        encoder: Encoder,
        output: EncoderOutput,
    ) -> Result<Self, String> {
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display.display,
                &NSArray::<SCWindow>::new(),
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(display.width);
            config.setHeight(display.height);
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
            config.setQueueDepth(5);
            config.setShowsCursor(true);
        }

        let handler = StreamOutput::new(encoder, output);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*handler)),
            )
        };
        let queue = DispatchQueue::new("nl.stormlantern.aetherdesk.capture", None);
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

/// Calls `start` with a completion block and waits for it.
fn wait(start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
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
