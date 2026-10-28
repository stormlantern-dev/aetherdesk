//! A stand-in for `aether-agent serve` that runs on Linux: encodes a moving
//! test pattern with ffmpeg's libx264 and speaks the real protocol. For
//! testing the client without a Mac:
//!
//!     cargo run --example fake_host -- [--port 4500] [--pair-code 123456]
//!         [--frames N] [--size 1280x720] [--fps 30] [--switch-every SECS]
//!     cargo run --bin aetherdesk -- pair 127.0.0.1 --code 123456 --name fake
//!     cargo run --bin aetherdesk -- fake
//!
//! It makes a new identity on every start and keeps paired clients in
//! memory, so pair again after restarting it (`aetherdesk pair` replaces the
//! old entry). Pairing stays open with `--pair-code` (default 123456) for 3
//! wrong codes. With `--frames N` it exits after one session of N frames.
//!
//! `--close-code N` ends the session with that QUIC close code (default 0,
//! "bye"); 4 is what a Mac host sends when it shuts down.
//!
//! `--switch-every` pretends the Mac goes back and forth between the login
//! window and the desktop, to see the client's view-only state.

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

    use aetherdesk_proto::trust::{Fingerprint, Identity, Role, start_pake};
    use aetherdesk_proto::{
        CLOSE_NOT_PAIRED, CLOSE_PAIRING_CLOSED, CLOSE_WRONG_CODE, ClientControl, Hello,
        HostControl, HostTiming, InputEvent, Intent, PROTOCOL_VERSION, PairMsg, SessionKind,
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
        let code = get("--pair-code")
            .cloned()
            .unwrap_or_else(|| "123456".into());
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
            let id = Identity::generate()?;
            let host = id.fingerprint();
            let endpoint = quic::server_endpoint(SocketAddr::from(([0, 0, 0, 0], port)), &id)
                .map_err(|e| anyhow!("{e}"))?;
            println!(
                "fake host {} on UDP {port}, pairing code {code}, {w}x{h}@{fps}",
                host.short()
            );
            let mut pairing = Pairing {
                code,
                tries_left: 3,
                paired: Vec::new(),
            };
            while let Some(incoming) = endpoint.accept().await {
                let Ok(conn) = incoming.await else { continue };
                println!("{}: connected", conn.remote_address());
                let (send, recv) = match admit(&conn, &mut pairing, &host).await {
                    Ok(Some(streams)) => streams,
                    Ok(None) => continue, // paired
                    Err(e) => {
                        println!("  refused: {e:#}");
                        continue;
                    }
                };
                match session(&conn, send, recv, frames, format, switch_every, close_code).await {
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

    /// Pairing state, in memory.
    struct Pairing {
        code: String,
        tries_left: u32,
        paired: Vec<Fingerprint>,
    }

    /// The host's side of the handshake, like `session::admit` on the Mac:
    /// the control stream for a paired client, `None` after a pairing.
    async fn admit(
        conn: &quinn::Connection,
        p: &mut Pairing,
        host: &Fingerprint,
    ) -> anyhow::Result<Option<(quinn::SendStream, quinn::RecvStream)>> {
        let client = quic::peer_fingerprint(conn).context("no client certificate")?;
        let (mut send, mut recv) = conn.accept_bi().await?;
        let hello: Hello = quic::recv(&mut recv).await?.context("no hello")?;
        match hello.intent {
            Intent::Connect if p.paired.contains(&client) => Ok(Some((send, recv))),
            Intent::Connect => {
                conn.close(CLOSE_NOT_PAIRED.into(), b"not paired");
                bail!("client {} isn't paired", client.short());
            }
            Intent::Pair { name } => {
                if p.tries_left == 0 {
                    conn.close(CLOSE_PAIRING_CLOSED.into(), b"pairing closed");
                    bail!("pairing is closed");
                }
                let Some(PairMsg::Spake(theirs)) = quic::recv(&mut recv).await? else {
                    bail!("expected a pairing message");
                };
                let (pake, ours) = start_pake(&p.code);
                quic::send(&mut send, &PairMsg::Spake(ours)).await?;
                let key = pake.finish(&theirs).ok();
                let Some(PairMsg::Confirm(mac)) = quic::recv(&mut recv).await? else {
                    bail!("expected a confirmation");
                };
                let Some(key) = key.filter(|k| k.verify(Role::Client, &client, host, &mac)) else {
                    p.tries_left -= 1;
                    conn.close(CLOSE_WRONG_CODE.into(), b"wrong code");
                    bail!("wrong code from {name:?} ({} tries left)", p.tries_left);
                };
                p.paired.push(client);
                quic::send(
                    &mut send,
                    &PairMsg::Paired {
                        name: "fake host".into(),
                        confirm: key.confirm(Role::Host, &client, host),
                    },
                )
                .await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                println!("  paired with {name} ({})", client.short());
                Ok(None)
            }
        }
    }

    async fn session(
        conn: &quinn::Connection,
        mut ctl_send: quinn::SendStream,
        mut ctl_recv: quinn::RecvStream,
        limit: Option<u64>,
        format: Format,
        switch_every: Option<u64>,
        close_code: u32,
    ) -> anyhow::Result<()> {
        let Format { w, h, fps } = format;
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
