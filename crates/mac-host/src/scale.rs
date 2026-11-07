//! Our own downscale, for `AUROVAL_CAPTURE_SCALE` (#113): the capture
//! comes in bigger than the picture and VideoToolbox's pixel transfer
//! (hardware, like the encoder) scales it with a box filter
//! (`kVTDownsamplingMode_Average`) instead of ScreenCaptureKit's scaler,
//! which has no filter choice.

use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferPool, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};
use objc2_video_toolbox::{
    VTPixelTransferSession, VTSessionSetProperty, kVTDownsamplingMode_Average,
    kVTDownsamplingMode_Decimate, kVTPixelTransferPropertyKey_DownsamplingMode,
    kVTPixelTransferPropertyKey_RealTime,
};

use crate::tuning::Downsample;

pub struct Scaler {
    session: CFRetained<VTPixelTransferSession>,
    pool: CFRetained<CVPixelBufferPool>,
}

// SAFETY: both are thread-safe Core Foundation objects; the capture queue
// is the only user.
unsafe impl Send for Scaler {}
unsafe impl Sync for Scaler {}

impl Scaler {
    /// Scales to `size`, keeping `pixel_format`.
    pub fn new(size: (usize, usize), pixel_format: u32, mode: Downsample) -> Result<Self, String> {
        let mut raw: *mut VTPixelTransferSession = ptr::null_mut();
        let status = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut raw)) };
        if status != 0 {
            return Err(format!(
                "VTPixelTransferSessionCreate failed: OSStatus {status}"
            ));
        }
        let session =
            unsafe { CFRetained::from_raw(NonNull::new(raw).ok_or("no pixel transfer session")?) };
        let downsampling: &CFString = unsafe {
            match mode {
                Downsample::Average => kVTDownsamplingMode_Average,
                Downsample::Decimate => kVTDownsamplingMode_Decimate,
            }
        };
        unsafe {
            for (key, value, name) in [
                (
                    kVTPixelTransferPropertyKey_DownsamplingMode,
                    downsampling as &CFType,
                    "DownsamplingMode",
                ),
                (
                    kVTPixelTransferPropertyKey_RealTime,
                    objc2_core_foundation::CFBoolean::new(true) as &CFType,
                    "RealTime",
                ),
            ] {
                let status = VTSessionSetProperty(&session, key, Some(value));
                if status != 0 {
                    eprintln!("note: pixel transfer ignores {name} (OSStatus {status})");
                }
            }
        }

        let format = CFNumber::new_i32(pixel_format as i32);
        let width = CFNumber::new_i32(size.0 as i32);
        let height = CFNumber::new_i32(size.1 as i32);
        // IOSurface-backed, so the encoder takes them without a copy.
        let iosurface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        let attributes = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&format, &width, &height, &iosurface],
            )
        };
        let mut pool: *mut CVPixelBufferPool = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                Some(attributes.as_opaque()),
                NonNull::from(&mut pool),
            )
        };
        if status != 0 {
            unsafe { session.invalidate() };
            return Err(format!("CVPixelBufferPoolCreate failed: {status}"));
        }
        let pool =
            unsafe { CFRetained::from_raw(NonNull::new(pool).ok_or("no pixel buffer pool")?) };
        Ok(Self { session, pool })
    }

    /// `image` at our size.
    pub fn scale(&self, image: &CVImageBuffer) -> Result<CFRetained<CVPixelBuffer>, String> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out))
        };
        if status != 0 {
            return Err(format!("no buffer from the pool ({status})"));
        }
        let out = unsafe { CFRetained::from_raw(NonNull::new(out).ok_or("no buffer")?) };
        let status = unsafe { self.session.transfer_image(image, &out) };
        if status != 0 {
            return Err(format!("scaling failed: OSStatus {status}"));
        }
        Ok(out)
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}
