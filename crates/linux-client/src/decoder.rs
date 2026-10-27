//! H.264 decoding with ffmpeg (software, for now) into planar YUV 4:2:0.

use aetherdesk_proto::VideoFrame;
use anyhow::{Context, bail};
use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

/// One decoded picture, planes copied out of ffmpeg's buffers.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Y, U, V.
    pub planes: [Vec<u8>; 3],
    pub strides: [usize; 3],
}

pub struct Decoder {
    inner: ffmpeg::decoder::Video,
    frame: ffmpeg::frame::Video,
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
            .context("this ffmpeg has no H.264 decoder")?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        ctx.set_flags(ffmpeg::codec::Flags::LOW_DELAY);
        // Frame threading buffers a frame per thread; slice threading adds no delay.
        let mut threading =
            ffmpeg::codec::threading::Config::kind(ffmpeg::codec::threading::Type::Slice);
        threading.count = 0;
        ctx.set_threading(threading);
        let inner = ctx.decoder().video()?;
        Ok(Self {
            inner,
            frame: ffmpeg::frame::Video::empty(),
        })
    }

    /// Decodes one access unit. Usually yields one picture; none while the
    /// decoder is still waiting for a keyframe.
    pub fn decode(
        &mut self,
        frame: &VideoFrame,
        mut out: impl FnMut(Picture),
    ) -> anyhow::Result<()> {
        let packet = ffmpeg::Packet::copy(&frame.data);
        self.inner.send_packet(&packet)?;
        while self.inner.receive_frame(&mut self.frame).is_ok() {
            let f = &self.frame;
            if !matches!(f.format(), Pixel::YUV420P | Pixel::YUVJ420P) {
                bail!("unexpected decoder output format {:?}", f.format());
            }
            let plane = |i: usize| f.data(i)[..f.stride(i) * f.plane_height(i) as usize].to_vec();
            out(Picture {
                width: f.width(),
                height: f.height(),
                planes: [plane(0), plane(1), plane(2)],
                strides: [f.stride(0), f.stride(1), f.stride(2)],
            });
        }
        Ok(())
    }
}
