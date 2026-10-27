use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDataProvider, CGEvent, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGImage,
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess, CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

const USAGE: &str = "\
usage:
  m0 info                      show permissions and session state, request permissions
  m0 probe [options]           capture every second, type text after a delay

probe options:
  --out DIR          output dir (default /Users/Shared/aetherdesk-m0/<time>-uid<uid>)
  --seconds N        how long to capture (default 60)
  --type-after N     seconds before typing; 0 = don't type (default 15)
  --text STR         lowercase a-z only, never followed by Return (default aetherdesk)
  --tap hid|session  where to post key events (default hid)";

struct Opts {
    out: Option<PathBuf>,
    seconds: u64,
    type_after: u64,
    text: String,
    tap: CGEventTapLocation,
}

pub fn run(args: Vec<String>) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("info") => {
            info();
            Ok(())
        }
        Some("probe") => probe(parse_opts(&args[1..])?),
        _ => Err(USAGE.into()),
    }
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        out: None,
        seconds: 60,
        type_after: 15,
        text: "aetherdesk".into(),
        tap: CGEventTapLocation::HIDEventTap,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--out" => o.out = Some(val()?.into()),
            "--seconds" => o.seconds = val()?.parse().map_err(|e| format!("--seconds: {e}"))?,
            "--type-after" => {
                o.type_after = val()?.parse().map_err(|e| format!("--type-after: {e}"))?
            }
            "--text" => o.text = val()?,
            "--tap" => {
                o.tap = match val()?.as_str() {
                    "hid" => CGEventTapLocation::HIDEventTap,
                    "session" => CGEventTapLocation::SessionEventTap,
                    t => return Err(format!("unknown tap {t}")),
                }
            }
            _ => return Err(format!("unknown option {flag}\n\n{USAGE}")),
        }
    }
    if let Some(c) = o.text.chars().find(|c| keycode(*c).is_none()) {
        return Err(format!("--text: unsupported character {c:?} (only a-z)"));
    }
    Ok(o)
}

// ---------------------------------------------------------------- logging

#[derive(Clone)]
struct Log(Arc<Mutex<Option<BufWriter<File>>>>, Instant);

impl Log {
    fn line(&self, msg: impl AsRef<str>) {
        let line = format!("[{:7.2}s] {}", self.1.elapsed().as_secs_f32(), msg.as_ref());
        println!("{line}");
        if let Some(f) = self.0.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn uid() -> (u32, u32) {
    unsafe extern "C" {
        fn getuid() -> u32;
        fn geteuid() -> u32;
    }
    unsafe { (getuid(), geteuid()) }
}

fn session_state() -> String {
    match CGSessionCopyCurrentDictionary() {
        Some(d) => format!("{d:?}"),
        None => "none (no window server session for this process)".into(),
    }
}

fn state_summary() -> String {
    format!(
        "screen-capture-access={} post-event-access={} secure-input={}",
        CGPreflightScreenCaptureAccess(),
        CGPreflightPostEventAccess(),
        unsafe { IsSecureEventInputEnabled() } != 0
    )
}

// ---------------------------------------------------------------- info

fn info() {
    let (uid, euid) = uid();
    println!("uid={uid} euid={euid}");
    println!("{}", state_summary());
    println!("session: {}", session_state());
    if !CGPreflightScreenCaptureAccess() {
        println!(
            "requesting Screen Recording access: {}",
            CGRequestScreenCaptureAccess()
        );
    }
    if !CGPreflightPostEventAccess() {
        println!(
            "requesting Accessibility (post event) access: {}",
            CGRequestPostEventAccess()
        );
    }
}

// ---------------------------------------------------------------- probe

fn probe(o: Opts) -> Result<(), String> {
    let (uid, euid) = uid();
    let out = o.out.clone().unwrap_or_else(|| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        PathBuf::from(format!("/Users/Shared/aetherdesk-m0/{ts}-uid{euid}"))
    });
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let file = File::create(out.join("log.txt")).map_err(|e| format!("log.txt: {e}"))?;
    let log = Log(
        Arc::new(Mutex::new(Some(BufWriter::new(file)))),
        Instant::now(),
    );

    log.line(format!("m0 probe → {}", out.display()));
    log.line(format!(
        "uid={uid} euid={euid} seconds={} type-after={} text-len={} tap={:?}",
        o.seconds,
        o.type_after,
        o.text.len(),
        o.tap
    ));
    log.line(state_summary());
    let mut last_session = session_state();
    log.line(format!("session: {last_session}"));

    if o.type_after > 0 {
        let (log, text, tap, delay) = (log.clone(), o.text.clone(), o.tap, o.type_after);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(delay));
            type_text(&log, &text, tap);
        });
    }

    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(o.seconds) {
        let tick = Instant::now();
        let session = session_state();
        if session != last_session {
            log.line(format!("session changed: {session}"));
            last_session = session;
        }
        let path = out.join(format!("frame-{n:04}.png"));
        match capture(&path) {
            Ok((w, h)) => log.line(format!("frame {n:04} ok {w}x{h} | {}", state_summary())),
            Err(e) => log.line(format!("frame {n:04} FAILED: {e} | {}", state_summary())),
        }
        n += 1;
        thread::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed()));
    }
    log.line("done");
    Ok(())
}

// ---------------------------------------------------------------- input

/// ANSI virtual keycodes (Carbon kVK_ANSI_*) for a-z.
fn keycode(c: char) -> Option<u16> {
    Some(match c {
        'a' => 0,
        's' => 1,
        'd' => 2,
        'f' => 3,
        'h' => 4,
        'g' => 5,
        'z' => 6,
        'x' => 7,
        'c' => 8,
        'v' => 9,
        'b' => 11,
        'q' => 12,
        'w' => 13,
        'e' => 14,
        'r' => 15,
        'y' => 16,
        't' => 17,
        'o' => 31,
        'u' => 32,
        'i' => 34,
        'p' => 35,
        'l' => 37,
        'j' => 38,
        'k' => 40,
        'n' => 45,
        'm' => 46,
        _ => return None,
    })
}

fn type_text(log: &Log, text: &str, tap: CGEventTapLocation) {
    log.line(format!(
        "typing {} chars via {tap:?} | {}",
        text.len(),
        state_summary()
    ));
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    if source.is_none() {
        log.line("CGEventSourceCreate returned null; posting without a source");
    }
    for c in text.chars() {
        let code = keycode(c).expect("validated in parse_opts");
        for down in [true, false] {
            match CGEvent::new_keyboard_event(source.as_deref(), code, down) {
                Some(ev) => CGEvent::post(tap, Some(&ev)),
                None => {
                    log.line(format!("CGEventCreateKeyboardEvent failed for {c:?}"));
                    return;
                }
            }
            thread::sleep(Duration::from_millis(40));
        }
    }
    log.line(format!("typed | {}", state_summary()));
}

// ---------------------------------------------------------------- capture

/// Lets a value cross from the completion-handler queue to the waiting thread.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

const TIMEOUT: Duration = Duration::from_secs(5);

fn ns_error(err: *mut NSError) -> String {
    match unsafe { err.as_ref() } {
        Some(e) => format!("{} (code {})", e.localizedDescription(), e.code()),
        None => "unknown error".into(),
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let res = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(ns_error(err)),
        };
        let _ = tx.send(res);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(TIMEOUT) {
        Ok(r) => r
            .map(|b| b.0)
            .map_err(|e| format!("SCShareableContent: {e}")),
        Err(_) => Err("SCShareableContent: timed out".into()),
    }
}

fn capture(path: &Path) -> Result<(usize, usize), String> {
    let content = shareable_content()?;
    let displays = unsafe { content.displays() };
    let display = displays.firstObject().ok_or("no displays")?;

    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::<SCWindow>::new(),
        )
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(display.width() as usize);
        config.setHeight(display.height() as usize);
        config.setShowsCursor(true);
    }

    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |img: *mut CGImage, err: *mut NSError| {
        let res = match std::ptr::NonNull::new(img) {
            Some(p) => Ok(SendBox(unsafe { CFRetained::retain(p) })),
            None => Err(ns_error(err)),
        };
        let _ = tx.send(res);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&block),
        )
    };
    let img = match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(b)) => b.0,
        Ok(Err(e)) => return Err(format!("captureImage: {e}")),
        Err(_) => return Err("captureImage: timed out".into()),
    };
    save_png(&img, path)
}

fn save_png(img: &CGImage, path: &Path) -> Result<(usize, usize), String> {
    let (w, h) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    let stride = CGImage::bytes_per_row(Some(img));
    if CGImage::bits_per_pixel(Some(img)) != 32 {
        return Err(format!(
            "unexpected {} bpp",
            CGImage::bits_per_pixel(Some(img))
        ));
    }
    let provider = CGImage::data_provider(Some(img)).ok_or("no data provider")?;
    let data = CGDataProvider::data(Some(&provider))
        .ok_or("no image data")?
        .to_vec();

    // SCK screenshots are BGRA (SDR); PNG wants RGBA.
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in data.chunks(stride).take(h) {
        for px in row[..w * 4].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok((w, h))
}
