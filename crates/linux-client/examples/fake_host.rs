//! A stand-in for `aether-agent serve` that runs on Linux: encodes a moving
//! test pattern with ffmpeg's libx264 and speaks the real protocol. For
//! testing the client without a Mac:
//!
//!     cargo run --example fake_host -- [--port 4500] [--token test] [--frames N]
//!     cargo run --bin aetherdesk -- 127.0.0.1 --token test

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = linux::main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use aetherdesk_proto::quic;
    use aetherdesk_proto::{
        ClientControl, Hello, InputEvent, PROTOCOL_VERSION, VideoFrame, Welcome,
    };
    use anyhow::{Context, anyhow, bail};
    use ffmpeg_next as ffmpeg;

    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: i32 = 30;

    pub fn main() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let get = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
        };
        let port: u16 = get("--port").map_or(Ok(4500), |p| p.parse())?;
        let token = get("--token").cloned().unwrap_or_else(|| "test".into());
        let frames: Option<u64> = get("--frames").map(|n| n.parse()).transpose()?;

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], port)))
                .map_err(|e| anyhow!("{e}"))?;
            println!("fake host on UDP {port}, token {token:?}, {W}x{H}@{FPS}");
            while let Some(incoming) = endpoint.accept().await {
                let conn = incoming.await?;
                println!("{}: connected", conn.remote_address());
                match session(&conn, &token, frames).await {
                    Ok(()) => println!("disconnected"),
                    Err(e) => println!("session ended: {e:#}"),
                }
                conn.close(0u32.into(), b"bye");
                if frames.is_some() {
                    endpoint.wait_idle().await;
                    break;
                }
            }
            Ok(())
        })
    }

    async fn session(
        conn: &quinn::Connection,
        token: &str,
        limit: Option<u64>,
    ) -> anyhow::Result<()> {
        let (mut ctl_send, mut ctl_recv) = conn.accept_bi().await?;
        let hello: Hello = quic::recv(&mut ctl_recv).await?.context("no hello")?;
        if hello.token != token {
            conn.close(2u32.into(), b"wrong token");
            bail!("wrong token");
        }
        let welcome = Welcome {
            version: PROTOCOL_VERSION,
            width: W,
            height: H,
        };
        quic::send(&mut ctl_send, &welcome).await?;
        let mut video = conn.open_uni().await?;

        let (kf_tx, mut kf_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(ClientControl::RequestKeyframe)) = quic::recv(&mut ctl_recv).await {
                println!("  client asked for a keyframe");
                let _ = kf_tx.send(());
            }
        });
        // Print input events (mouse moves are too chatty to show).
        let input_conn = conn.clone();
        tokio::spawn(async move {
            let Ok(mut stream) = input_conn.accept_uni().await else {
                return;
            };
            while let Ok(Some(event)) = quic::recv::<InputEvent>(&mut stream).await {
                if !matches!(event, InputEvent::MouseMove { .. }) {
                    println!("  input: {event:?}");
                }
            }
        });

        let mut encoder = Encoder::new()?;
        let start = Instant::now();
        let mut n = 0u64;
        loop {
            if limit.is_some_and(|l| n >= l) {
                video.finish()?;
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                return Ok(());
            }
            let force_key = kf_rx.try_recv().is_ok();
            for (data, keyframe) in encoder.encode(n, force_key)? {
                let pts_us = n * 1_000_000 / FPS as u64;
                if quic::send(
                    &mut video,
                    &VideoFrame {
                        pts_us,
                        keyframe,
                        data,
                    },
                )
                .await
                .is_err()
                {
                    return Ok(());
                }
            }
            n += 1;
            let due = start + Duration::from_micros(n * 1_000_000 / FPS as u64);
            tokio::time::sleep_until(due.into()).await;
        }
    }

    struct Encoder {
        inner: ffmpeg::encoder::Video,
        frame: ffmpeg::frame::Video,
    }

    impl Encoder {
        fn new() -> anyhow::Result<Self> {
            ffmpeg::init()?;
            let codec =
                ffmpeg::encoder::find_by_name("libx264").context("ffmpeg has no libx264")?;
            let mut enc = ffmpeg::codec::context::Context::new_with_codec(codec)
                .encoder()
                .video()?;
            enc.set_width(W);
            enc.set_height(H);
            enc.set_format(ffmpeg::format::Pixel::YUV420P);
            enc.set_time_base((1, FPS));
            enc.set_frame_rate(Some((FPS, 1)));
            let mut opts = ffmpeg::Dictionary::new();
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
            let inner = enc.open_with(opts)?;
            let frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, W, H);
            Ok(Self { inner, frame })
        }

        /// A diagonal gradient that scrolls, plus a bar that sweeps across.
        fn encode(&mut self, n: u64, keyframe: bool) -> anyhow::Result<Vec<(Vec<u8>, bool)>> {
            let t = n as usize;
            let (w, h) = (W as usize, H as usize);
            let stride = self.frame.stride(0);
            let y = self.frame.data_mut(0);
            for row in 0..h {
                for col in 0..w {
                    let bar = (col + w - (t * 8) % w) % w < 40;
                    y[row * stride + col] = if bar {
                        235
                    } else {
                        ((row + col + t * 4) % 200 + 16) as u8
                    };
                }
            }
            for plane in 1..3 {
                let value = if plane == 1 { (t % 256) as u8 } else { 128 };
                self.frame.data_mut(plane).fill(value);
            }
            self.frame.set_pts(Some(n as i64));
            self.frame.set_kind(if keyframe {
                ffmpeg::picture::Type::I
            } else {
                ffmpeg::picture::Type::None
            });
            self.inner.send_frame(&self.frame)?;
            let mut out = Vec::new();
            let mut packet = ffmpeg::Packet::empty();
            while self.inner.receive_packet(&mut packet).is_ok() {
                if let Some(data) = packet.data() {
                    out.push((data.to_vec(), packet.is_key()));
                }
            }
            Ok(out)
        }
    }
}
