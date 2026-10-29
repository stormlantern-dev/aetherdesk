//! The Mac's pointer, drawn as a real cursor on our side so it follows the
//! hand without the round trip. The host sends each shape once
//! ([`HostControl::CursorImage`]) and then names it; until the first one
//! arrives, or after [`HostControl::CursorInVideo`], the pointer is in the
//! video and ours stays hidden while grabbed.

use std::collections::HashMap;

use aetherdesk_proto::{CursorImage, HostControl};
use sdl3::mouse::Cursor;
use sdl3::pixels::PixelFormat;
use sdl3::surface::Surface;

#[derive(Default)]
pub struct Pointer {
    images: HashMap<u64, CursorImage>,
    /// The shape on the Mac now; `None`: the pointer is in the video.
    current: Option<u64>,
    /// The SDL cursor for `current` at a scale and density, reused while
    /// those stay the same.
    built: Option<(u64, (f32, f32), Cursor)>,
}

/// What a message means for the window.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// The shape (or where it's drawn) changed: show the pointer again.
    Shape,
    /// The Mac moved the pointer: put ours there (`0.0..=1.0` over the
    /// picture).
    MoveTo(f32, f32),
    Nothing,
}

impl Pointer {
    pub fn handle(&mut self, msg: HostControl) -> Change {
        match msg {
            HostControl::CursorImage(image) => {
                self.current = Some(image.id);
                self.images.insert(image.id, image);
                Change::Shape
            }
            HostControl::CursorShow(id) if self.images.contains_key(&id) => {
                self.current = Some(id);
                Change::Shape
            }
            HostControl::CursorInVideo => {
                self.current = None;
                Change::Shape
            }
            HostControl::CursorMoved { x, y } => Change::MoveTo(x, y),
            _ => Change::Nothing,
        }
    }

    /// Whether we draw the pointer (else it's in the video).
    pub fn local(&self) -> bool {
        self.current.is_some()
    }

    /// The cursor to show, scaled from the Mac's pixels by `scale` (window
    /// points per picture pixel), with a sharper image for `density`
    /// (window pixels per point).
    pub fn cursor(&mut self, scale: f32, density: f32) -> Option<&Cursor> {
        let id = self.current?;
        let fresh = matches!(&self.built, Some((b, s, _)) if *b == id && *s == (scale, density));
        if !fresh {
            let image = self.images.get(&id)?;
            match build(image, scale, density) {
                Ok(c) => self.built = Some((id, (scale, density), c)),
                Err(e) => {
                    warn!("pointer shape: {e}");
                    return None;
                }
            }
        }
        self.built.as_ref().map(|(_, _, c)| c)
    }
}

fn scaled(src: &Surface, w: f32, h: f32) -> Result<Surface<'static>, String> {
    let (w, h) = ((w.round() as i32).max(1), (h.round() as i32).max(1));
    let raw = unsafe {
        sdl3::sys::surface::SDL_ScaleSurface(
            src.raw(),
            w,
            h,
            sdl3::sys::surface::SDL_SCALEMODE_LINEAR,
        )
    };
    if raw.is_null() {
        return Err(sdl3::get_error().to_string());
    }
    Ok(unsafe { Surface::from_ll(raw) })
}

fn build(image: &CursorImage, scale: f32, density: f32) -> Result<Cursor, String> {
    let (w, h) = (image.width, image.height);
    let mut rgba = image.rgba.clone();
    if rgba.len() != (w * h * 4) as usize {
        return Err("image size doesn't match its data".into());
    }
    let src = Surface::from_data(&mut rgba, w, h, w * 4, PixelFormat::RGBA32)
        .map_err(|e| e.to_string())?;
    // SDL takes the base image at 100 % display scale (window points) and
    // an alternate one for denser screens.
    let base = scaled(&src, w as f32 * scale, h as f32 * scale)?;
    if density > 1.01 {
        let sharp = scaled(&src, w as f32 * scale * density, h as f32 * scale * density)?;
        unsafe { sdl3::sys::surface::SDL_AddSurfaceAlternateImage(base.raw(), sharp.raw()) };
    }
    let hot = |v: u32| (v as f32 * scale).round() as i32;
    Cursor::from_surface(&base, hot(image.hot_x), hot(image.hot_y)).map_err(|e| e.to_string())
}

/// Where a point on the picture (`0.0..=1.0`) is in the window, in points,
/// with the picture letterboxed into `window` (points) from `picture`
/// (pixels). Also the picture's scale: window points per picture pixel.
pub fn place(x: f32, y: f32, window: (u32, u32), picture: (u32, u32)) -> ((f32, f32), f32) {
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let (pw, ph) = (picture.0.max(1) as f32, picture.1.max(1) as f32);
    let scale = (ww / pw).min(wh / ph);
    let (ox, oy) = ((ww - pw * scale) / 2.0, (wh - ph * scale) / 2.0);
    ((ox + x * pw * scale, oy + y * ph * scale), scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: u64) -> CursorImage {
        CursorImage {
            id,
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0, 0, 0, 255],
        }
    }

    #[test]
    fn shapes_are_kept_and_named() {
        let mut p = Pointer::default();
        assert!(!p.local(), "in the video until a shape arrives");
        assert_eq!(
            p.handle(HostControl::CursorShow(1)),
            Change::Nothing,
            "unknown id"
        );
        assert_eq!(p.handle(HostControl::CursorImage(image(1))), Change::Shape);
        assert_eq!(p.handle(HostControl::CursorImage(image(2))), Change::Shape);
        assert_eq!(p.handle(HostControl::CursorShow(1)), Change::Shape);
        assert_eq!(p.current, Some(1));
        assert_eq!(p.handle(HostControl::CursorInVideo), Change::Shape);
        assert!(!p.local());
        assert_eq!(
            p.handle(HostControl::CursorMoved { x: 0.5, y: 0.25 }),
            Change::MoveTo(0.5, 0.25)
        );
    }

    #[test]
    fn places_follow_the_letterbox() {
        // A 3456×2234 picture in a 1000×1000 window: bars top and bottom.
        let ((x, y), s) = place(0.0, 0.0, (1000, 1000), (3456, 2234));
        assert!((s - 1000.0 / 3456.0).abs() < 1e-6);
        assert_eq!(x, 0.0);
        assert!((y - (1000.0 - 2234.0 * s) / 2.0).abs() < 1e-3);
        let ((x, _), _) = place(1.0, 0.5, (1000, 1000), (3456, 2234));
        assert!((x - 1000.0).abs() < 1e-3);
    }
}
