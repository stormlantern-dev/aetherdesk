//! The Mac's mouse pointer, for the client to draw locally: its shape, and
//! where it is when the Mac moved it by itself.
//!
//! With this the video is captured without the pointer, and the client shows
//! a real cursor that follows the hand without the round trip. It relies on
//! `NSCursor.currentSystemCursor`, which Apple marks as going away ("always
//! nil in a future version of macOS"); when it's gone [`available`] says
//! so and the pointer simply stays in the video.

use std::collections::HashSet;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aetherdesk_proto::{CursorImage, HostControl};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext, CGEvent, CGImage};

use crate::input::{Point, Rect};

/// How often the shape and position are read. Shape changes (an I-beam
/// over text) show up within this.
const POLL: Duration = Duration::from_millis(30);

/// No moves from the client for this long, and the Mac's pointer is
/// elsewhere: the Mac moved it itself.
const QUIET: Duration = Duration::from_millis(250);

/// How far (in points) the Mac's pointer may be from where we put it before
/// it counts as moved.
const SLACK: f64 = 4.0;

/// Where the client last put the pointer, and when. Shared with the
/// injector.
pub type LastMove = Arc<Mutex<Option<(Point, Instant)>>>;

/// Whether the shape can be read here at all. Not at the login window, and
/// not on a macOS that dropped `currentSystemCursor`.
pub fn available() -> bool {
    autoreleasepool(|_| system_cursor().is_some())
}

/// The cursor on screen, whichever app set it. Deprecated with no
/// replacement for other apps' cursors: this is the one place we use it,
/// and [`available`] turns the local pointer off when it returns nothing.
#[allow(deprecated)]
fn system_cursor() -> Option<objc2::rc::Retained<NSCursor>> {
    NSCursor::currentSystemCursor()
}

/// A running watcher; dropping it stops it.
pub struct Watcher {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Sends the shape whenever it changes (each image once, then by id)
    /// and the position when the Mac moved the pointer itself.
    pub fn start(
        display: Rect,
        last_move: LastMove,
        send: impl Fn(HostControl) + Send + 'static,
    ) -> Self {
        let (stop, stopped) = channel::<()>();
        let thread = std::thread::Builder::new()
            .name("cursor".into())
            .spawn(move || {
                let mut sent_images = HashSet::new();
                let mut shown = None;
                let mut reported: Option<Point> = None;
                loop {
                    match stopped.recv_timeout(POLL) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    if let Some(image) = autoreleasepool(|_| current_image())
                        && shown != Some(image.id)
                    {
                        shown = Some(image.id);
                        if sent_images.insert(image.id) {
                            send(HostControl::CursorImage(image));
                        } else {
                            send(HostControl::CursorShow(image.id));
                        }
                    }
                    let at = location();
                    let last = *last_move.lock().unwrap();
                    let moved_by_mac = match last {
                        Some((put, when)) => when.elapsed() > QUIET && far(at, put),
                        None => true, // nothing from the client yet: say where it is
                    };
                    if moved_by_mac && reported.is_none_or(|r| far(at, r)) {
                        reported = Some(at);
                        let (x, y) = normalize(at, display);
                        send(HostControl::CursorMoved { x, y });
                    }
                    if !moved_by_mac {
                        reported = None;
                    }
                }
            })
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn far(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() > SLACK || (a.y - b.y).abs() > SLACK
}

fn normalize(p: Point, d: Rect) -> (f32, f32) {
    (
        ((p.x - d.x) / d.width).clamp(0.0, 1.0) as f32,
        ((p.y - d.y) / d.height).clamp(0.0, 1.0) as f32,
    )
}

/// The pointer's position in global display points.
fn location() -> Point {
    let event = CGEvent::new(None);
    let p = CGEvent::location(event.as_deref());
    Point { x: p.x, y: p.y }
}

unsafe extern "C-unwind" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<NonNull<CGContext>>;
}

/// `kCGImageAlphaPremultipliedLast` | `kCGBitmapByteOrder32Big`: R, G, B, A
/// in memory order.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);

/// The current system cursor as straight-alpha RGBA, at the display's pixel
/// density (2× on a Retina Mac), with its hotspot in the same pixels.
fn current_image() -> Option<CursorImage> {
    let cursor = system_cursor()?;
    let image = cursor.image();
    let points = image.size();
    let cg =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let (w, h) = (CGImage::width(Some(&cg)), CGImage::height(Some(&cg)));
    if w == 0 || h == 0 || w > 512 || h > 512 || points.width <= 0.0 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let space = CGColorSpace::new_device_rgb()?;
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            RGBA_PREMULTIPLIED,
        )
    }?;
    let ctx = unsafe { CFRetained::from_raw(ctx) };
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(&cg));
    drop(ctx);
    // Premultiplied → straight alpha, as SDL expects.
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let scale = w as f64 / points.width;
    let hot = cursor.hotSpot();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (w, h, hot.x.to_bits(), hot.y.to_bits()).hash(&mut hasher);
    rgba.hash(&mut hasher);
    Some(CursorImage {
        id: hasher.finish(),
        width: w as u32,
        height: h as u32,
        hot_x: (hot.x * scale).round() as u32,
        hot_y: (hot.y * scale).round() as u32,
        rgba,
    })
}
