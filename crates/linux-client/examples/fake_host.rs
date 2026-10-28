//! A stand-in for `aether-agent serve` that runs on Linux: encodes a moving
//! test pattern with ffmpeg's libx264 and speaks the real protocol. For
//! testing the client without a Mac:
//!
//!     cargo run --example fake_host -- [--port 4500] [--token test] [--frames N]
//!         [--size 1280x720] [--fps 30] [--switch-every SECS]
//!
//! `--close-code N` ends the session with that QUIC close code (default 0,
//! "bye"); 4 is what a Mac host sends when it shuts down.
//!
//! `--switch-every` pretends the Mac goes back and forth between the login
//! window and the desktop, to see the client's view-only state.
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
    use std::sync::OnceLock;

    use aetherdesk_proto::{
        ClientControl, Hello, HostControl, HostTiming, InputEvent, PROTOCOL_VERSION, SessionKind,
        VideoFrame, Welcome,
    };
    use anyhow::{Context, anyhow, bail};
    use ffmpeg_next as ffmpeg;

    /// Picture size and rate, from `--size` and `--fps`.
    #[derive(Clone, Copy)]
    struct Format {
        w: u32,
        h: u32,
        fps: u32,
    }

    /// Our "host clock": microseconds since start.
    fn now_us() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
    }

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
        let (w, h) = match get("--size") {
            Some(s) => {
                let (w, h) = s.split_once('x').context("--size wants WxH")?;
                (w.parse()?, h.parse()?)
            }
            None => (1280, 720),
        };
        let fps: u32 = get("--fps").map_or(Ok(30), |f| f.parse())?;
        let format = Format { w, h, fps };
        let switch_every: Option<u64> = get("--switch-every").map(|n| n.parse()).transpose()?;
        let close_code: u32 = get("--close-code").map_or(Ok(0), |c| c.parse())?;

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], port)))
                .map_err(|e| anyhow!("{e}"))?;
            println!("fake host on UDP {port}, token {token:?}, {w}x{h}@{fps}");
            while let Some(incoming) = endpoint.accept().await {
                let conn = incoming.await?;
                println!("{}: connected", conn.remote_address());
                match session(&conn, &token, frames, format, switch_every, close_code).await {
                    Ok(()) => println!("disconnected"),
                    Err(e) => println!("session ended: {e:#}"),
                }
                conn.close(close_code.into(), b"bye");
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
        format: Format,
        switch_every: Option<u64>,
        close_code: u32,
    ) -> anyhow::Result<()> {
        let Format { w, h, fps } = format;
        let (mut ctl_send, mut ctl_recv) = conn.accept_bi().await?;
        let hello: Hello = quic::recv(&mut ctl_recv).await?.context("no hello")?;
        if hello.token != token {
            conn.close(2u32.into(), b"wrong token");
            bail!("wrong token");
        }
        let welcome = Welcome {
            version: PROTOCOL_VERSION,
            width: w,
            height: h,
        };
        quic::send(&mut ctl_send, &welcome).await?;
        let mut video = conn.open_uni().await?;

        // One writer for the control stream; pongs and session changes
        // queue up for it.
        let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<HostControl>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if quic::send(&mut ctl_send, &msg).await.is_err() {
                    break;
                }
            }
        });
        let _ = out.send(HostControl::Session(SessionKind::User));
        if let Some(secs) = switch_every {
            let out = out.clone();
            tokio::spawn(async move {
                let mut kind = SessionKind::User;
                loop {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    kind = match kind {
                        SessionKind::User => SessionKind::LoginWindow,
                        SessionKind::LoginWindow => SessionKind::User,
                    };
                    println!("  pretending: {kind:?}");
                    if out.send(HostControl::Session(kind)).is_err() {
                        break;
                    }
                }
            });
        }
        let (kf_tx, mut kf_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = quic::recv(&mut ctl_recv).await {
                match msg {
                    ClientControl::RequestKeyframe => {
                        println!("  client asked for a keyframe");
                        let _ = kf_tx.send(());
                    }
                    ClientControl::Ping { client_us } => {
                        let pong = HostControl::Pong {
                            client_us,
                            host_us: now_us(),
                        };
                        if out.send(pong).is_err() {
                            break;
                        }
                    }
                }
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

        let mut encoder = Encoder::new(format)?;
        let start = Instant::now();
        let mut n = 0u64;
        loop {
            if limit.is_some_and(|l| n >= l) {
                if close_code != 0 {
                    // Like a host shutting down: close right away, mid-stream.
                    return Ok(());
                }
                video.finish()?;
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                return Ok(());
            }
            let force_key = kf_rx.try_recv().is_ok();
            let captured_us = now_us();
            let packets = encoder.encode(n, force_key)?;
            let encoded_us = now_us();
            for (data, keyframe) in packets {
                let pts_us = n * 1_000_000 / u64::from(fps);
                let timing = HostTiming {
                    captured_us,
                    encoded_us,
                    sent_us: now_us(),
                };
                if quic::send(
                    &mut video,
                    &VideoFrame {
                        pts_us,
                        keyframe,
                        timing,
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
            let due = start + Duration::from_micros(n * 1_000_000 / u64::from(fps));
            tokio::time::sleep_until(due.into()).await;
        }
    }

    struct Encoder {
        inner: ffmpeg::encoder::Video,
        frame: ffmpeg::frame::Video,
        w: usize,
        h: usize,
    }

    impl Encoder {
        fn new(Format { w, h, fps }: Format) -> anyhow::Result<Self> {
            let fps = fps as i32;
            ffmpeg::init()?;
            let codec =
                ffmpeg::encoder::find_by_name("libx264").context("ffmpeg has no libx264")?;
            let mut enc = ffmpeg::codec::context::Context::new_with_codec(codec)
                .encoder()
                .video()?;
            enc.set_width(w);
            enc.set_height(h);
            enc.set_format(ffmpeg::format::Pixel::YUV420P);
            enc.set_time_base((1, fps));
            enc.set_frame_rate(Some((fps, 1)));
            let mut opts = ffmpeg::Dictionary::new();
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
            let inner = enc.open_with(opts)?;
            let frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);
            Ok(Self {
                inner,
                frame,
                w: w as usize,
                h: h as usize,
            })
        }

        /// A diagonal gradient that scrolls, plus a bar that sweeps across.
        fn encode(&mut self, n: u64, keyframe: bool) -> anyhow::Result<Vec<(Vec<u8>, bool)>> {
            let t = n as usize;
            let (w, h) = (self.w, self.h);
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
