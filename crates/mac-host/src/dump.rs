//! `AUROVAL_DUMP_FRAME=PREFIX`: saves the first frame of each size that
//! goes into the encoder as `PREFIX-captured-WxH.png` (and, with
//! `AUROVAL_CAPTURE_SCALE`, the bigger capture before our downscale),
//! for comparing with what the client decodes (#113). Off unless set; then
//! the capture path doesn't look at it.

use std::sync::Mutex;

use auroval_proto::still::{self, Pixels};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

pub struct Dump {
    prefix: String,
    /// Sizes already saved.
    done: Mutex<Vec<(usize, usize)>>,
}

impl Dump {
    /// A dump when `AUROVAL_DUMP_FRAME` is set.
    pub fn from_env() -> Option<Self> {
        let prefix = crate::tuning::Tuning::get().dump.clone()?;
        Some(Self {
            prefix,
            done: Mutex::new(Vec::new()),
        })
    }

    /// Saves `image` if it's the first of its size. The pixels are copied
    /// here; converting and writing happen on another thread.
    pub fn offer(&self, image: &CVImageBuffer) {
        let size = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        {
            let mut done = self.done.lock().unwrap();
            if done.contains(&size) {
                return;
            }
            done.push(size);
        }
        let copy = match unsafe { Copied::from(image) } {
            Ok(c) => c,
            Err(e) => {
                eprintln!("dump: {e}");
                return;
            }
        };
        let path = still::path(&self.prefix, "captured", (size.0 as u32, size.1 as u32));
        std::thread::spawn(move || {
            let rgb = still::to_rgb(&copy.pixels(), size.0, size.1);
            match still::write_png(&path, size.0 as u32, size.1 as u32, &rgb) {
                Ok(()) => println!("dump: saved {}", path.display()),
                Err(e) => eprintln!("dump: {e}"),
            }
        });
    }
}

/// A pixel buffer's planes, copied out.
enum Copied {
    Nv12 {
        y: (Vec<u8>, usize),
        uv: (Vec<u8>, usize),
    },
    Bgra((Vec<u8>, usize)),
}

impl Copied {
    /// # Safety
    /// `image` must be a pixel buffer (ScreenCaptureKit's, or ours).
    unsafe fn from(image: &CVImageBuffer) -> Result<Self, String> {
        let flags = CVPixelBufferLockFlags::ReadOnly;
        let status = unsafe { CVPixelBufferLockBaseAddress(image, flags) };
        if status != 0 {
            return Err(format!("locking the frame failed ({status})"));
        }
        let plane = |i: usize| unsafe {
            let base = CVPixelBufferGetBaseAddressOfPlane(image, i).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image, i);
            let len = stride * CVPixelBufferGetHeightOfPlane(image, i);
            (std::slice::from_raw_parts(base, len).to_vec(), stride)
        };
        let result = match CVPixelBufferGetPixelFormatType(image) {
            f if f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange => Ok(Copied::Nv12 {
                y: plane(0),
                uv: plane(1),
            }),
            f if f == kCVPixelFormatType_32BGRA => unsafe {
                let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRow(image);
                let len = stride * CVPixelBufferGetHeight(image);
                Ok(Copied::Bgra((
                    std::slice::from_raw_parts(base, len).to_vec(),
                    stride,
                )))
            },
            f => Err(format!("can't save pixel format {f:#x}")),
        };
        unsafe { CVPixelBufferUnlockBaseAddress(image, flags) };
        result
    }

    fn pixels(&self) -> Pixels<'_> {
        match self {
            Copied::Nv12 { y, uv } => Pixels::Nv12 {
                y: (&y.0, y.1),
                uv: (&uv.0, uv.1),
            },
            Copied::Bgra(p) => Pixels::Bgra((&p.0, p.1)),
        }
    }
}
