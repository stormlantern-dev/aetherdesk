//! Video decoding with ffmpeg: on the GPU when there is one (CUDA, then
//! VA-API), in software otherwise. 4:2:0 pictures come out as they are,
//! planar (I420) or with interleaved chroma (NV12, what the GPU hands back);
//! 4:4:4 ones are turned into BGRA here, since SDL has no 4:4:4 texture.

use std::ptr;
use std::sync::OnceLock;

use aetherdesk_proto::{Codec, VideoFrame};
use anyhow::{Context, bail};
use ffmpeg::ffi;
use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

use crate::latency::{Timing, now_us};

/// How the planes of a [`Picture`] are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Y, U, V.
    I420,
    /// Y, then U and V interleaved; the third plane is empty.
    Nv12,
    /// One plane, 4 bytes per pixel: B, G, R, A.
    Bgra,
}

/// One decoded picture, planes copied out of ffmpeg's buffers.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    pub planes: [Vec<u8>; 3],
    pub strides: [usize; 3],
    pub timing: Timing,
}

/// Hardware decoding ffmpeg can do on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gpu {
    Cuda,
    Vaapi,
}

impl Gpu {
    fn device_type(self) -> ffi::AVHWDeviceType {
        match self {
            Gpu::Cuda => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            Gpu::Vaapi => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Gpu::Cuda => "CUDA",
            Gpu::Vaapi => "VA-API",
        }
    }

    /// A new device of this kind, or `None` when there's none here.
    fn open(self) -> Option<*mut ffi::AVBufferRef> {
        let mut device = ptr::null_mut();
        let status = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                self.device_type(),
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        (status >= 0 && !device.is_null()).then_some(device)
    }
}

/// The GPU to decode on, if any. `AETHERDESK_HWDEC=0` turns it off.
pub fn gpu() -> Option<Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    *GPU.get_or_init(|| {
        if std::env::var("AETHERDESK_HWDEC").is_ok_and(|v| v == "0") {
            return None;
        }
        let _ = ffmpeg::init();
        // A missing GPU isn't an error worth ffmpeg's log lines.
        let level = ffmpeg::util::log::get_level();
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);
        let found = [Gpu::Cuda, Gpu::Vaapi]
            .into_iter()
            .find(|g| match g.open() {
                Some(mut device) => {
                    unsafe { ffi::av_buffer_unref(&mut device) };
                    true
                }
                None => false,
            });
        if let Ok(level) = level {
            ffmpeg::util::log::set_level(level);
        }
        found
    })
}

/// The format asked for with `--video`, if any.
static PREFERRED: OnceLock<Codec> = OnceLock::new();

/// Asks for `codec` first (with H.264 behind it, for a host that can't).
pub fn prefer(codec: Codec) {
    let _ = PREFERRED.set(codec);
}

/// The video formats this client offers the host, best first. Without a
/// choice: HEVC 4:2:0 with a GPU to decode it, else H.264 (in software HEVC
/// costs more than it gains). 4:4:4 only when asked for: until the picture
/// stays on the GPU it costs ~15 ms more on the client.
/// `AETHERDESK_CODECS=hevc,h264` overrides everything (for tests).
pub fn codecs() -> &'static [Codec] {
    static CODECS: OnceLock<Vec<Codec>> = OnceLock::new();
    CODECS.get_or_init(|| {
        if let Ok(list) = std::env::var("AETHERDESK_CODECS") {
            let parsed: Vec<Codec> = list
                .split(',')
                .filter_map(|c| parse_codec(c.trim()))
                .collect();
            if !parsed.is_empty() {
                return parsed;
            }
        }
        match (PREFERRED.get(), gpu()) {
            (Some(&Codec::H264), _) => vec![Codec::H264],
            (Some(&c), _) => vec![c, Codec::H264],
            (None, Some(_)) => vec![Codec::Hevc, Codec::H264],
            (None, None) => vec![Codec::H264],
        }
    })
}

pub fn parse_codec(s: &str) -> Option<Codec> {
    match s.to_ascii_lowercase().as_str() {
        "h264" => Some(Codec::H264),
        "hevc" => Some(Codec::Hevc),
        "hevc444" => Some(Codec::Hevc444),
        _ => None,
    }
}

unsafe extern "C" fn pick_cuda(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe { pick(formats, ffi::AVPixelFormat::AV_PIX_FMT_CUDA) }
}

unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe { pick(formats, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI) }
}

/// ffmpeg's `get_format`: the GPU format when offered, else its first
/// choice (software).
unsafe fn pick(formats: *const ffi::AVPixelFormat, want: ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    let mut p = formats;
    unsafe {
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == want {
                return want;
            }
            p = p.add(1);
        }
        *formats
    }
}

pub struct Decoder {
    inner: ffmpeg::decoder::Video,
    frame: ffmpeg::frame::Video,
    /// Where GPU frames are copied to.
    downloaded: ffmpeg::frame::Video,
    /// For 4:4:4 pictures, made on the first one.
    to_bgra: Option<ToBgra>,
}

/// Turns 4:4:4 pictures into BGRA on all cores, using the colour matrix and
/// range the stream is tagged with.
struct ToBgra {
    ctx: *mut ffi::SwsContext,
    out: ffmpeg::frame::Video,
}

impl ToBgra {
    fn new() -> anyhow::Result<Self> {
        let ctx = unsafe { ffi::sws_alloc_context() };
        if ctx.is_null() {
            bail!("swscale: no context");
        }
        unsafe { (*ctx).threads = 0 }; // one per core
        Ok(Self {
            ctx,
            out: ffmpeg::frame::Video::empty(),
        })
    }

    fn convert(&mut self, src: &ffmpeg::frame::Video) -> anyhow::Result<&ffmpeg::frame::Video> {
        if (self.out.width(), self.out.height()) != (src.width(), src.height()) {
            self.out = ffmpeg::frame::Video::new(Pixel::BGRA, src.width(), src.height());
        }
        let status = unsafe { ffi::sws_scale_frame(self.ctx, self.out.as_mut_ptr(), src.as_ptr()) };
        if status < 0 {
            bail!("converting a 4:4:4 picture failed ({status})");
        }
        Ok(&self.out)
    }
}

impl Drop for ToBgra {
    fn drop(&mut self) {
        unsafe { ffi::sws_free_context(&mut self.ctx) };
    }
}

impl Decoder {
    pub fn new(codec: Codec) -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let id = match codec {
            Codec::H264 => ffmpeg::codec::Id::H264,
            Codec::Hevc | Codec::Hevc444 => ffmpeg::codec::Id::HEVC,
        };
        let found = ffmpeg::decoder::find(id)
            .with_context(|| format!("this ffmpeg can't decode {codec}"))?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(found);
        ctx.set_flags(ffmpeg::codec::Flags::LOW_DELAY);
        // Frame threading buffers a frame per thread; slice threading adds no delay.
        let mut threading =
            ffmpeg::codec::threading::Config::kind(ffmpeg::codec::threading::Type::Slice);
        threading.count = 0;
        ctx.set_threading(threading);
        let on = match gpu().and_then(|g| g.open().map(|d| (g, d))) {
            Some((g, device)) => {
                unsafe {
                    let raw = ctx.as_mut_ptr();
                    // The context owns the reference from here on.
                    (*raw).hw_device_ctx = device;
                    (*raw).get_format = Some(match g {
                        Gpu::Cuda => pick_cuda,
                        Gpu::Vaapi => pick_vaapi,
                    });
                }
                format!("on the GPU ({})", g.name())
            }
            None => "in software".to_string(),
        };
        println!("decoding {codec} {on}");
        let inner = ctx.decoder().video()?;
        Ok(Self {
            inner,
            frame: ffmpeg::frame::Video::empty(),
            downloaded: ffmpeg::frame::Video::empty(),
            to_bgra: None,
        })
    }

    /// Decodes one access unit. Usually yields one picture; none while the
    /// decoder is still waiting for a keyframe.
    pub fn decode(
        &mut self,
        frame: &VideoFrame,
        timing: Timing,
        mut out: impl FnMut(Picture),
    ) -> anyhow::Result<()> {
        let packet = ffmpeg::Packet::copy(&frame.data);
        self.inner.send_packet(&packet)?;
        while self.inner.receive_frame(&mut self.frame).is_ok() {
            let on_gpu = matches!(self.frame.format(), Pixel::CUDA | Pixel::VAAPI);
            let f = if on_gpu {
                let status = unsafe {
                    ffi::av_hwframe_transfer_data(
                        self.downloaded.as_mut_ptr(),
                        self.frame.as_ptr(),
                        0,
                    )
                };
                if status < 0 {
                    bail!("copying a picture from the GPU failed ({status})");
                }
                &self.downloaded
            } else {
                &self.frame
            };
            let (f, layout) = match f.format() {
                Pixel::YUV420P | Pixel::YUVJ420P => (f, Layout::I420),
                Pixel::NV12 => (f, Layout::Nv12),
                Pixel::YUV444P | Pixel::YUVJ444P => {
                    if self.to_bgra.is_none() {
                        self.to_bgra = Some(ToBgra::new()?);
                    }
                    let to_bgra = self.to_bgra.as_mut().expect("just made");
                    (to_bgra.convert(f)?, Layout::Bgra)
                }
                other => bail!("unexpected decoder output format {other:?}"),
            };
            let plane = |i: usize| f.data(i)[..f.stride(i) * f.plane_height(i) as usize].to_vec();
            let (planes, strides) = match layout {
                Layout::I420 => (
                    [plane(0), plane(1), plane(2)],
                    [f.stride(0), f.stride(1), f.stride(2)],
                ),
                Layout::Nv12 => (
                    [plane(0), plane(1), Vec::new()],
                    [f.stride(0), f.stride(1), 0],
                ),
                Layout::Bgra => ([plane(0), Vec::new(), Vec::new()], [f.stride(0), 0, 0]),
            };
            out(Picture {
                width: f.width(),
                height: f.height(),
                layout,
                planes,
                strides,
                timing: Timing {
                    decoded: now_us(),
                    ..timing
                },
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_parse() {
        assert_eq!(parse_codec("HEVC"), Some(Codec::Hevc));
        assert_eq!(parse_codec("h264"), Some(Codec::H264));
        assert_eq!(parse_codec("hevc444"), Some(Codec::Hevc444));
        assert_eq!(parse_codec("av1"), None);
    }
}
