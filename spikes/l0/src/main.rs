//! S5: how long does a Hyprland host take from a rendered frame to an
//! encoded bitstream?
//!
//! Captures one output with ext-image-copy-capture into shared memory and
//! encodes every frame with NVENC through ffmpeg, timing each stage:
//!
//! - copy:    compositor presented the frame → our `ready` event
//! - upload:  shared memory → ffmpeg frame (CPU memcpy)
//! - convert: BGRX → YUV 4:4:4 on the CPU (444 only)
//! - encode:  send_frame → packet out of NVENC
//! - total:   presented → packet
//!
//! Usage: l0 [--output NAME] [--frames N] [--codec hevc|hevc444|h264] [--mbit N]
//! Run it against a compositor in WAYLAND_DISPLAY, with something animating
//! on that output (otherwise capture waits for damage forever).
//!
//! Two more modes for the lock-screen spikes:
//!
//! - S1: `l0 --snapshot FILE.ppm [--output NAME]` captures one frame and
//!   writes it as a PPM, to see what a locked output gives.
//! - S2: `l0 --type TEXT [--enter]` types lowercase letters and digits
//!   through zwp_virtual_keyboard_v1 (US layout), optionally followed by
//!   Enter, to see whether the lock screen gets them.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as source, ext_output_image_capture_source_manager_v1 as source_mgr,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as vk_mgr, zwp_virtual_keyboard_v1 as vk,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as capture_mgr,
    ext_image_copy_capture_session_v1 as session,
};

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    sources: Option<source_mgr::ExtOutputImageCaptureSourceManagerV1>,
    capture: Option<capture_mgr::ExtImageCopyCaptureManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboards: Option<vk_mgr::ZwpVirtualKeyboardManagerV1>,
    outputs: Vec<(wl_output::WlOutput, String)>,
    size: Option<(u32, u32)>,
    formats: Vec<u32>,
    constraints_done: bool,
    stopped: bool,
    presented_ns: Option<u64>,
    ready: bool,
    failed: Option<u32>,
}

fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let frames: usize = arg("--frames").and_then(|s| s.parse().ok()).unwrap_or(150);
    let codec = arg("--codec").unwrap_or_else(|| "hevc".into());
    let mbit: usize = arg("--mbit").and_then(|s| s.parse().ok()).unwrap_or(20);
    let want_output = arg("--output");

    let conn = Connection::connect_to_env().expect("no Wayland compositor in WAYLAND_DISPLAY");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st).unwrap();
    queue.roundtrip(&mut st).unwrap(); // output names

    if let Some(text) = arg("--type") {
        type_text(&conn, &mut queue, &mut st, &text, std::env::args().any(|a| a == "--enter"));
        return;
    }
    let snapshot = arg("--snapshot");

    let shm = st.shm.clone().expect("no wl_shm");
    let sources = st.sources.clone().expect("compositor lacks ext_output_image_capture_source_manager_v1");
    let capture = st.capture.clone().expect("compositor lacks ext_image_copy_capture_manager_v1");
    for (_, n) in &st.outputs {
        eprintln!("output: {n}");
    }
    let (output, name) = match &want_output {
        Some(w) => st.outputs.iter().find(|(_, n)| n == w).cloned().expect("no such output"),
        None => st.outputs.first().cloned().expect("no outputs"),
    };

    let src = sources.create_source(&output, &qh, ());
    let sess = capture.create_session(&src, capture_mgr::Options::empty(), &qh, ());
    while !st.constraints_done {
        queue.blocking_dispatch(&mut st).unwrap();
    }
    let (w, h) = st.size.expect("no buffer size");
    // XRGB8888 / ARGB8888 are BGRX / BGRA in memory on little-endian.
    let (shm_fmt, pix) = if st.formats.contains(&(wl_shm::Format::Xrgb8888 as u32)) {
        (wl_shm::Format::Xrgb8888, Pixel::BGRZ)
    } else if st.formats.contains(&(wl_shm::Format::Argb8888 as u32)) {
        (wl_shm::Format::Argb8888, Pixel::BGRA)
    } else {
        panic!("no XRGB/ARGB shm format offered: {:?}", st.formats)
    };
    eprintln!("capturing {name} at {w}x{h}, shm {shm_fmt:?}, codec {codec}, {mbit} Mbit/s");

    // Shared memory buffer.
    let stride = w as usize * 4;
    let len = stride * h as usize;
    let fd = unsafe {
        let fd = libc::memfd_create(c"l0".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0);
        assert_eq!(libc::ftruncate(fd, len as i64), 0);
        OwnedFd::from_raw_fd(fd)
    };
    let map = unsafe {
        use std::os::fd::AsRawFd;
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0);
        assert_ne!(p, libc::MAP_FAILED);
        std::slice::from_raw_parts(p as *const u8, len)
    };
    let pool = shm.create_pool(fd.as_fd(), len as i32, &qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, shm_fmt, &qh, ());

    if let Some(path) = snapshot {
        let f = sess.create_frame(&qh, ());
        f.attach_buffer(&buffer);
        f.damage_buffer(0, 0, w as i32, h as i32);
        f.capture();
        while !st.ready && st.failed.is_none() && !st.stopped {
            queue.blocking_dispatch(&mut st).unwrap();
        }
        if let Some(r) = st.failed {
            panic!("capture failed, reason {r}");
        }
        assert!(!st.stopped, "capture session stopped");
        // BGRX/BGRA → RGB PPM, and say how dark it is (a black frame is the
        // failure S1 looks for).
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        let mut sum = 0u64;
        for px in map.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
            sum += px[0] as u64 + px[1] as u64 + px[2] as u64;
        }
        std::fs::write(&path, out).unwrap();
        let mean = sum as f64 / (w as f64 * h as f64 * 3.0);
        println!("wrote {path}: {w}x{h}, mean brightness {mean:.1} of 255");
        return;
    }

    // Encoder.
    ffmpeg::init().unwrap();
    let (enc_name, enc_pix, profile) = match codec.as_str() {
        "h264" => ("h264_nvenc", pix, None),
        "hevc" => ("hevc_nvenc", pix, None),
        "hevc444" => ("hevc_nvenc", Pixel::YUV444P, Some("rext")),
        other => panic!("unknown codec {other}"),
    };
    let enc_codec = ffmpeg::encoder::find_by_name(enc_name).expect("encoder not in this ffmpeg");
    let mut enc = ffmpeg::codec::context::Context::new_with_codec(enc_codec).encoder().video().unwrap();
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(enc_pix);
    enc.set_time_base((1, 60));
    enc.set_frame_rate(Some((60, 1)));
    enc.set_bit_rate(mbit * 1_000_000);
    enc.set_max_b_frames(0);
    enc.set_gop(600);
    let mut opts = ffmpeg::Dictionary::new();
    for (k, v) in [("preset", "p1"), ("tune", "ull"), ("zerolatency", "1"), ("delay", "0"), ("rc", "cbr")] {
        opts.set(k, v);
    }
    if let Some(p) = profile {
        opts.set("profile", p);
    }
    let mut enc = enc.open_with(opts).expect("opening the encoder failed");
    let mut src_frame = ffmpeg::frame::Video::new(pix, w, h);
    let mut yuv = ffmpeg::frame::Video::new(Pixel::YUV444P, w, h);
    let mut sws = (enc_pix == Pixel::YUV444P).then(|| {
        ffmpeg::software::scaling::Context::get(pix, w, h, Pixel::YUV444P, w, h, ffmpeg::software::scaling::Flags::POINT)
            .unwrap()
    });
    let mut packet = ffmpeg::Packet::empty();

    #[derive(Clone, Copy)]
    struct Row {
        copy: f64,
        upload: f64,
        convert: f64,
        encode: f64,
        total: f64,
        bytes: usize,
    }
    let warmup = 10;
    let mut rows = Vec::new();
    for i in 0..frames + warmup {
        st.ready = false;
        st.failed = None;
        st.presented_ns = None;
        let f = sess.create_frame(&qh, ());
        f.attach_buffer(&buffer);
        f.damage_buffer(0, 0, w as i32, h as i32);
        f.capture();
        while !st.ready && st.failed.is_none() && !st.stopped {
            queue.blocking_dispatch(&mut st).unwrap();
        }
        let t_ready = now_ns();
        f.destroy();
        if let Some(r) = st.failed {
            panic!("capture failed, reason {r}");
        }
        if st.stopped {
            panic!("capture session stopped");
        }
        let presented = st.presented_ns.unwrap_or(t_ready);

        // Shared memory → ffmpeg frame.
        let fs = src_frame.stride(0);
        {
            let data = src_frame.data_mut(0);
            for y in 0..h as usize {
                data[y * fs..y * fs + stride].copy_from_slice(&map[y * stride..(y + 1) * stride]);
            }
        }
        let t_up = now_ns();
        let input = if let Some(s) = sws.as_mut() {
            s.run(&src_frame, &mut yuv).unwrap();
            &mut yuv
        } else {
            &mut src_frame
        };
        input.set_pts(Some(i as i64));
        let t_conv = now_ns();
        enc.send_frame(input).unwrap();
        let bytes;
        loop {
            match enc.receive_packet(&mut packet) {
                Ok(()) => {
                    bytes = packet.size();
                    break;
                }
                Err(ffmpeg::Error::Other { errno: libc::EAGAIN }) => std::thread::sleep(Duration::from_micros(50)),
                Err(e) => panic!("encode: {e}"),
            }
        }
        let t_pkt = now_ns();
        if i >= warmup {
            let ms = |a: u64, b: u64| b.saturating_sub(a) as f64 / 1e6;
            rows.push(Row {
                copy: ms(presented, t_ready),
                upload: ms(t_ready, t_up),
                convert: ms(t_up, t_conv),
                encode: ms(t_conv, t_pkt),
                total: ms(presented, t_pkt),
                bytes,
            });
        }
    }

    let stat = |f: fn(&Row) -> f64| {
        let mut v: Vec<f64> = rows.iter().map(f).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[v.len() * 95 / 100])
    };
    println!("{codec} {w}x{h}, {} frames (after {warmup} warm-up), ms median / p95:", rows.len());
    for (label, f) in [
        ("copy (presented → ready)", (|r: &Row| r.copy) as fn(&Row) -> f64),
        ("upload (shm → frame)", |r: &Row| r.upload),
        ("convert (BGRX → 4:4:4, CPU)", |r: &Row| r.convert),
        ("encode (NVENC)", |r: &Row| r.encode),
        ("total (presented → packet)", |r: &Row| r.total),
    ] {
        let (m, p) = stat(f);
        println!("  {label:<30} {m:7.2} / {p:7.2}");
    }
    let avg = rows.iter().map(|r| r.bytes).sum::<usize>() / rows.len();
    println!("  average packet {:.1} kB", avg as f64 / 1000.0);
    let _ = (pool, src);
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "wl_output" => {
                    let o = reg.bind(name, version.min(4), qh, ());
                    st.outputs.push((o, String::new()));
                }
                "ext_output_image_capture_source_manager_v1" => st.sources = Some(reg.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => st.capture = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, 1, qh, ())),
                "zwp_virtual_keyboard_manager_v1" => st.keyboards = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

/// Evdev key code for a lowercase letter or digit (US layout).
fn evdev_key(c: char) -> u32 {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    ];
    match c {
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        '1'..='9' => c as u32 - '1' as u32 + 2,
        '0' => 11,
        _ => panic!("--type takes lowercase letters and digits only, got {c:?}"),
    }
}

fn type_text(conn: &Connection, queue: &mut wayland_client::EventQueue<State>, st: &mut State, text: &str, enter: bool) {
    let qh = queue.handle();
    let seat = st.seat.clone().expect("no wl_seat");
    let mgr = st.keyboards.clone().expect("compositor lacks zwp_virtual_keyboard_manager_v1");
    let kb = mgr.create_virtual_keyboard(&seat, &qh, ());

    // The protocol wants an XKB keymap before any key; let xkbcommon's own
    // tool compile a US one.
    let keymap = std::process::Command::new("xkbcli")
        .args(["compile-keymap", "--layout", "us"])
        .output()
        .expect("xkbcli not found");
    assert!(keymap.status.success(), "xkbcli compile-keymap failed");
    let mut km = keymap.stdout;
    km.push(0);
    let fd = unsafe {
        let fd = libc::memfd_create(c"keymap".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0);
        OwnedFd::from_raw_fd(fd)
    };
    let mut file = std::fs::File::from(fd);
    std::io::Write::write_all(&mut file, &km).unwrap();
    kb.keymap(1 /* XKB_V1 */, file.as_fd(), km.len() as u32);
    queue.roundtrip(st).unwrap();

    let mut keys: Vec<u32> = text.chars().map(evdev_key).collect();
    if enter {
        keys.push(28);
    }
    let start = now_ns();
    for k in keys {
        let t = ((now_ns() - start) / 1_000_000) as u32;
        kb.key(t, k, 1);
        kb.key(t + 1, k, 0);
        conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
    queue.roundtrip(st).unwrap();
    println!("typed {} keys{}", text.len(), if enter { " + Enter" } else { "" });
    kb.destroy();
    conn.flush().unwrap();
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(st: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            if let Some(e) = st.outputs.iter_mut().find(|(x, _)| x == o) {
                e.1 = name;
            }
        }
    }
}

impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(st: &mut Self, _: &session::ExtImageCopyCaptureSessionV1, ev: session::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            session::Event::BufferSize { width, height } => st.size = Some((width, height)),
            session::Event::ShmFormat { format: WEnum::Value(f) } => st.formats.push(f as u32),
            session::Event::ShmFormat { format: WEnum::Unknown(f) } => st.formats.push(f),
            session::Event::Done => st.constraints_done = true,
            session::Event::Stopped => st.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(st: &mut Self, _: &frame::ExtImageCopyCaptureFrameV1, ev: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            frame::Event::PresentationTime { tv_sec_hi, tv_sec_lo, tv_nsec } => {
                let secs = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                st.presented_ns = Some(secs * 1_000_000_000 + tv_nsec as u64);
            }
            frame::Event::Ready => st.ready = true,
            frame::Event::Failed { reason } => {
                st.failed = Some(match reason {
                    WEnum::Value(r) => r as u32,
                    WEnum::Unknown(r) => r,
                })
            }
            _ => {}
        }
    }
}

macro_rules! no_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
no_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    source_mgr::ExtOutputImageCaptureSourceManagerV1,
    source::ExtImageCaptureSourceV1,
    capture_mgr::ExtImageCopyCaptureManagerV1,
    wl_seat::WlSeat,
    vk_mgr::ZwpVirtualKeyboardManagerV1,
    vk::ZwpVirtualKeyboardV1
);
