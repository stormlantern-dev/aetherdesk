//! The SDL3 window: shows the newest decoded picture, letterboxed to the
//! remote display's aspect ratio, with a small overlay for what the title
//! bar can't show on a tiling compositor: whether input is local, and the
//! connection state.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use aetherdesk_proto::{ClipboardEcho, InputEvent, MAX_CLIPBOARD, Welcome};
use anyhow::{Context, anyhow};
use sdl3::clipboard::ClipboardUtil;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;
use sdl3::mouse::{Cursor, SystemCursor};
use sdl3::pixels::{Color, PixelFormat};
use sdl3::render::{BlendMode, FRect, WindowCanvas};
use sdl3::sys::render::SDL_RendererLogicalPresentation;
use tokio::sync::mpsc::UnboundedSender;

use crate::decoder::Picture;
use crate::input::Forwarder;
use crate::latency::{ClockSync, Stats, now_us};
use crate::net::State;
use crate::pointer::{Change, Pointer};

/// How long the "input → Mac" note shows after grabbing.
const TOAST: Duration = Duration::from_secs(2);

/// No frames or pongs for this long while connected: say so.
const STALLED_US: i64 = 1_500_000;

/// What the window needs from the link.
pub struct LinkView<'a> {
    pub sync: &'a ClockSync,
    pub login_window: &'a AtomicBool,
    pub state: &'a Mutex<State>,
    pub last_heard: &'a AtomicI64,
    pub retry: UnboundedSender<()>,
    /// Clipboard sync: our copies out, the Mac's in. `None` when off.
    pub clipboard: Option<(UnboundedSender<String>, Receiver<String>)>,
    /// The Mac's pointer shape and moves.
    pub pointer: Receiver<aetherdesk_proto::HostControl>,
}

pub fn run(
    welcome: &Welcome,
    host: &str,
    pictures: Receiver<Picture>,
    input: UnboundedSender<InputEvent>,
    link: LinkView<'_>,
) -> anyhow::Result<()> {
    let sdl = sdl3::init().map_err(|e| anyhow!("SDL: {e}"))?;
    let video = sdl.video().map_err(|e| anyhow!("SDL video: {e}"))?;
    let (w, h) = (welcome.width, welcome.height);
    let title = format!("AetherDesk — {host}");
    let window = video
        .window(&title, (w / 2).max(320), (h / 2).max(200))
        .resizable()
        .high_pixel_density()
        .build()
        .context("creating window")?;
    let mut canvas = window.into_canvas();
    canvas
        .set_logical_size(w, h, SDL_RendererLogicalPresentation::LETTERBOX)
        .map_err(|e| anyhow!("logical size: {e}"))?;
    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_streaming(PixelFormat::IYUV, w, h)
        .context("creating texture")?;
    let mut size = (w, h);
    let mut events = sdl.event_pump().map_err(|e| anyhow!("SDL events: {e}"))?;
    let mut forwarder = Forwarder::new(input);
    let mouse = sdl.mouse();
    let clipboard = video.clipboard();
    let mut echo = ClipboardEcho::default();
    // What's on our clipboard already stays here: only copies made during
    // the session go to the Mac.
    if link.clipboard.is_some()
        && let Ok(text) = clipboard.clipboard_text()
    {
        echo.noted(&text);
    }
    let default_cursor = Cursor::from_system(SystemCursor::Arrow).ok();
    let mut pointer = Pointer::default();
    // Grabbed: compositor shortcuts (Super+…) go to the Mac, and the pointer
    // is the Mac's: drawn here in its shape when the Mac sends one, hidden
    // when it's in the video. Released: our normal cursor. SDL shows the
    // normal one outside the window either way.
    let apply_grab = |canvas: &mut WindowCanvas, grabbed: bool, pointer: &mut Pointer, picture| {
        canvas.window_mut().set_keyboard_grab(grabbed);
        let window = canvas.window();
        let (_, scale) = crate::pointer::place(0.0, 0.0, window.size(), picture);
        match pointer.cursor(scale, window.pixel_density()) {
            Some(c) if grabbed => {
                c.set();
                mouse.show_cursor(true);
            }
            _ => {
                if let Some(c) = &default_cursor {
                    c.set();
                }
                mouse.show_cursor(!grabbed);
            }
        }
    };
    apply_grab(&mut canvas, true, &mut pointer, (w, h));
    // What the pointer was last sized for: the window and the picture.
    let mut shaped_for = (canvas.window().size(), (w, h));
    #[cfg(feature = "test-hooks")]
    let hooks = crate::testhooks::Hooks::start();

    let mut fps = 0.0;
    let mut at_login = link.login_window.load(Ordering::Relaxed);
    let mut state = State::Connected;
    // The grab before the connection dropped, to restore it afterwards.
    let mut grab_before_loss = true;
    let mut toast_until = Some(Instant::now() + TOAST);
    let mut drawn: Option<Overlay> = None;
    let mut last_draw = Instant::now();
    let mut have_picture = false;
    set_title(&mut canvas, &title, fps, true, at_login, &state);

    let mut latency = Stats::default();
    let mut shown = 0u32;
    let mut second = Instant::now();
    loop {
        // Connection state first: losing it releases the grab at once, so a
        // dead window never keeps Hyprland's shortcuts.
        let now_state = link.state.lock().unwrap().clone();
        if now_state != state {
            let was_connected = state == State::Connected;
            let connected = now_state == State::Connected;
            if was_connected && !connected {
                grab_before_loss = forwarder.grabbed();
                forwarder.release_keys();
                if forwarder.grabbed() {
                    forwarder.set_grabbed(false);
                    apply_grab(&mut canvas, false, &mut pointer, size);
                }
            } else if connected && !was_connected && grab_before_loss {
                forwarder.set_grabbed(true);
                apply_grab(&mut canvas, true, &mut pointer, size);
                toast_until = Some(Instant::now() + TOAST);
            }
            state = now_state;
            set_title(
                &mut canvas,
                &title,
                fps,
                forwarder.grabbed(),
                at_login,
                &state,
            );
        }
        let connected = state == State::Connected;

        #[allow(unused_mut)]
        let mut batch: Vec<Event> = events.poll_iter().collect();
        // Test commands: input joins the real events; snapshot and state
        // are answered after the next draw.
        #[cfg(feature = "test-hooks")]
        let mut after_draw: Vec<crate::testhooks::Request> = Vec::new();
        #[cfg(feature = "test-hooks")]
        if let Some(h) = &hooks {
            let id = canvas.window().id();
            for (command, reply) in h.pending() {
                match crate::testhooks::event(&command, id) {
                    Some(e) => {
                        batch.push(e);
                        let _ = reply.send("ok".into());
                    }
                    None => after_draw.push((command, reply)),
                }
            }
        }
        for event in batch {
            match event {
                Event::Quit { .. } => {
                    forwarder.release_keys();
                    return Ok(());
                }
                Event::ClipboardUpdate { .. } => {
                    if let Some((out, _)) = &link.clipboard {
                        copied_here(&clipboard, &mut echo, out);
                    }
                }
                // While disconnected the window is ours: Esc quits, Enter
                // retries now.
                Event::KeyDown {
                    scancode: Some(sc), ..
                } if !connected => match sc {
                    Scancode::Escape => return Ok(()),
                    Scancode::Return | Scancode::KpEnter => {
                        let _ = link.retry.send(());
                    }
                    _ => {}
                },
                _ if !connected => {}
                _ => {
                    if let Some(grabbed) = forwarder.handle(&event, &canvas, size) {
                        apply_grab(&mut canvas, grabbed, &mut pointer, size);
                        if grabbed {
                            toast_until = Some(Instant::now() + TOAST);
                        }
                        set_title(&mut canvas, &title, fps, grabbed, at_login, &state);
                    }
                }
            }
        }
        // The Mac's pointer: a new shape, or the Mac moved it.
        let mut reshape = (canvas.window().size(), size) != shaped_for;
        while let Ok(msg) = link.pointer.try_recv() {
            match pointer.handle(msg) {
                Change::Shape => reshape = true,
                Change::MoveTo(x, y) if forwarder.grabbed() && pointer.local() => {
                    let ((wx, wy), _) = crate::pointer::place(x, y, canvas.window().size(), size);
                    mouse.warp_mouse_in_window(canvas.window(), wx, wy);
                }
                _ => {}
            }
        }
        if reshape {
            shaped_for = (canvas.window().size(), size);
            apply_grab(&mut canvas, forwarder.grabbed(), &mut pointer, size);
        }
        if let Some((_, from_mac)) = &link.clipboard {
            while let Ok(text) = from_mac.try_recv() {
                copied_on_mac(&clipboard, &mut echo, &text);
            }
        }
        if link.login_window.load(Ordering::Relaxed) != at_login {
            at_login = !at_login;
            set_title(
                &mut canvas,
                &title,
                fps,
                forwarder.grabbed(),
                at_login,
                &state,
            );
        }

        let picture = match pictures.recv_timeout(Duration::from_millis(10)) {
            Ok(mut p) => {
                while let Ok(newer) = pictures.try_recv() {
                    p = newer;
                }
                Some(p)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                println!("connection closed");
                return Ok(());
            }
        };
        if let Some(picture) = &picture {
            if size != (picture.width, picture.height) {
                size = (picture.width, picture.height);
                texture = creator
                    .create_texture_streaming(PixelFormat::IYUV, size.0, size.1)
                    .context("resizing texture")?;
                canvas
                    .set_logical_size(size.0, size.1, SDL_RendererLogicalPresentation::LETTERBOX)
                    .map_err(|e| anyhow!("logical size: {e}"))?;
            }
            texture
                .update_yuv(
                    None,
                    &picture.planes[0],
                    picture.strides[0],
                    &picture.planes[1],
                    picture.strides[1],
                    &picture.planes[2],
                    picture.strides[2],
                )
                .context("uploading picture")?;
            have_picture = true;
        }

        if toast_until.is_some_and(|t| Instant::now() >= t) {
            toast_until = None;
        }
        let stalled = connected && now_us() - link.last_heard.load(Ordering::Relaxed) > STALLED_US;
        let overlay = Overlay::new(&state, forwarder.grabbed(), toast_until.is_some(), stalled);
        // Redraw on a new picture, or when the overlay changed; while it
        // shows something that counts, a few times a second is plenty.
        let changed = drawn.as_ref() != Some(&overlay);
        #[cfg(feature = "test-hooks")]
        let changed = changed || !after_draw.is_empty();
        if picture.is_some() || changed {
            canvas.set_draw_color(Color::RGB(0, 0, 0));
            canvas.clear();
            if have_picture {
                canvas
                    .copy(&texture, None, None)
                    .map_err(|e| anyhow!("drawing: {e}"))?;
            }
            overlay.draw(&mut canvas, size)?;
            #[cfg(feature = "test-hooks")]
            for (command, reply) in after_draw.drain(..) {
                let answer = match command {
                    crate::testhooks::Command::Snapshot(path) => {
                        canvas
                            .set_logical_size(0, 0, SDL_RendererLogicalPresentation::DISABLED)
                            .map_err(|e| anyhow!("snapshot: {e}"))?;
                        let result = crate::testhooks::snapshot(&canvas, &path);
                        canvas
                            .set_logical_size(
                                size.0,
                                size.1,
                                SDL_RendererLogicalPresentation::LETTERBOX,
                            )
                            .map_err(|e| anyhow!("snapshot: {e}"))?;
                        match result {
                            Ok((w, h)) => format!("ok {w}x{h}"),
                            Err(e) => format!("error: {e}"),
                        }
                    }
                    crate::testhooks::Command::State => state_json(
                        &state,
                        forwarder.grabbed(),
                        at_login,
                        &overlay,
                        canvas.output_size().unwrap_or_default(),
                        pointer.local(),
                    ),
                    // As if another app copied: set it, then the same path
                    // as a clipboard change.
                    crate::testhooks::Command::Copy(text) => {
                        match clipboard.set_clipboard_text(&text) {
                            Ok(()) => {
                                if let Some((out, _)) = &link.clipboard {
                                    copied_here(&clipboard, &mut echo, out);
                                }
                                "ok".into()
                            }
                            Err(e) => format!("error: {e}"),
                        }
                    }
                    crate::testhooks::Command::Paste => match clipboard.clipboard_text() {
                        Ok(text) => format!("ok {}", crate::testhooks::json_str(&text)),
                        Err(e) => format!("error: {e}"),
                    },
                    _ => "ok".into(),
                };
                let _ = reply.send(answer);
            }
            canvas.present();
            drawn = Some(overlay);
            last_draw = Instant::now();
        } else if last_draw.elapsed() > Duration::from_secs(1) {
            drawn = None; // keeps the window fresh after e.g. a resize
        }

        if let Some(picture) = picture {
            latency.shown(
                crate::latency::Timing {
                    presented: now_us(),
                    ..picture.timing
                },
                link.sync,
            );
            shown += 1;
        }
        if second.elapsed() >= Duration::from_secs(1) {
            fps = f64::from(shown) / second.elapsed().as_secs_f64();
            set_title(
                &mut canvas,
                &title,
                fps,
                forwarder.grabbed(),
                at_login,
                &state,
            );
            if shown > 0 {
                println!("  {fps:.0} fps shown");
            }
            shown = 0;
            second = Instant::now();
        }
    }
}

/// What to draw over the picture. Compared between frames to know when to
/// redraw.
#[derive(Debug, PartialEq, Eq)]
struct Overlay {
    /// Darken the picture: it's not live.
    dim: bool,
    /// Centered message lines.
    message: Vec<String>,
    /// Bottom-left notes, bottom line last.
    notes: Vec<String>,
}

impl Overlay {
    fn new(state: &State, grabbed: bool, toast: bool, stalled: bool) -> Self {
        let mut o = Overlay {
            dim: false,
            message: Vec::new(),
            notes: Vec::new(),
        };
        match state {
            State::Connected => {
                if stalled {
                    o.notes.push("Connection stalled...".into());
                }
                if !grabbed {
                    o.notes.push("LOCAL - Right Ctrl to grab".into());
                } else if toast {
                    o.notes.push("-> Mac - Right Ctrl releases".into());
                }
            }
            State::Reconnecting { attempt, reason } => {
                o.dim = true;
                o.message = vec![
                    format!("Connection lost - reconnecting (attempt {attempt})..."),
                    ascii(reason),
                    String::new(),
                    "Esc to quit".into(),
                ];
            }
            State::GaveUp { reason } => {
                o.dim = true;
                o.message = vec![
                    "Disconnected".into(),
                    ascii(reason),
                    String::new(),
                    "Enter to retry, Esc to quit".into(),
                ];
            }
        }
        o
    }

    fn draw(&self, canvas: &mut WindowCanvas, logical: (u32, u32)) -> anyhow::Result<()> {
        if !self.dim && self.message.is_empty() && self.notes.is_empty() {
            return Ok(());
        }
        // Draw in window pixels at a readable scale; with the logical size
        // on, the 8 px font would shrink with the 3456 px wide picture.
        canvas
            .set_logical_size(0, 0, SDL_RendererLogicalPresentation::DISABLED)
            .map_err(|e| anyhow!("overlay: {e}"))?;
        let (pw, ph) = canvas.output_size().map_err(|e| anyhow!("overlay: {e}"))?;
        let line = 12.0;
        let widest = self
            .message
            .iter()
            .chain(&self.notes)
            .map(|t| t.len())
            .max()
            .unwrap_or(0) as f32
            * 8.0;
        let scale = scale_for(pw as f32, ph as f32, widest);
        canvas
            .set_scale(scale, scale)
            .map_err(|e| anyhow!("overlay: {e}"))?;
        let (w, h) = (pw as f32 / scale, ph as f32 / scale);
        canvas.set_blend_mode(BlendMode::Blend);

        if self.dim {
            canvas.set_draw_color(Color::RGBA(0, 0, 0, 170));
            canvas
                .fill_rect(FRect::new(0.0, 0.0, w, h))
                .map_err(|e| anyhow!("overlay: {e}"))?;
        }
        let top = (h - self.message.len() as f32 * line) / 2.0;
        for (i, text) in self.message.iter().enumerate() {
            let x = ((w - text.len() as f32 * 8.0) / 2.0).max(4.0);
            canvas.set_draw_color(Color::RGB(240, 240, 240));
            canvas
                .draw_debug_text(text, (x, top + i as f32 * line))
                .map_err(|e| anyhow!("overlay: {e}"))?;
        }
        let (x, bottom) = notes_anchor(
            (w, h),
            picture_rect((w, h), logical),
            self.notes.iter().map(|t| t.len()).max().unwrap_or(0) as f32 * 8.0 + 8.0,
            self.notes.len() as f32 * (line + 6.0),
        );
        for (i, text) in self.notes.iter().rev().enumerate() {
            let y = bottom - 12.0 - i as f32 * (line + 6.0);
            let width = text.len() as f32 * 8.0;
            canvas.set_draw_color(Color::RGBA(0, 0, 0, 190));
            canvas
                .fill_rect(FRect::new(x, y - 4.0, width + 8.0, 16.0))
                .map_err(|e| anyhow!("overlay: {e}"))?;
            canvas.set_draw_color(Color::RGB(255, 210, 90));
            canvas
                .draw_debug_text(text, (x + 4.0, y))
                .map_err(|e| anyhow!("overlay: {e}"))?;
        }

        canvas.set_blend_mode(BlendMode::None);
        canvas
            .set_scale(1.0, 1.0)
            .map_err(|e| anyhow!("overlay: {e}"))?;
        canvas
            .set_logical_size(
                logical.0,
                logical.1,
                SDL_RendererLogicalPresentation::LETTERBOX,
            )
            .map_err(|e| anyhow!("overlay: {e}"))?;
        Ok(())
    }
}

/// Share of the Mac's screen height, from the bottom, kept clear for the
/// Dock when the notes have to go over the picture.
const DOCK_ZONE: f32 = 0.12;

/// Text scale for the 8 px debug font: grows with the window height
/// (2× at ~700 px, 3× at 1440), but never so big that the widest line
/// doesn't fit.
fn scale_for(pw: f32, ph: f32, widest: f32) -> f32 {
    let by_height = (ph / 450.0).round();
    let by_width = if widest > 0.0 {
        ((pw - 24.0) / widest).floor()
    } else {
        by_height
    };
    by_height.min(by_width).clamp(1.0, 4.0)
}

/// Where the letterboxed picture sits in a `(w, h)` window: x, y, width,
/// height.
fn picture_rect((w, h): (f32, f32), (lw, lh): (u32, u32)) -> (f32, f32, f32, f32) {
    let s = (w / lw as f32).min(h / lh as f32);
    let (pw, ph) = (lw as f32 * s, lh as f32 * s);
    ((w - pw) / 2.0, (h - ph) / 2.0, pw, ph)
}

/// Bottom-left anchor (x, bottom edge) for the notes: in the black bar
/// below or beside the picture when it's big enough, so they cover nothing
/// of the Mac's screen; otherwise in the picture's left edge, just above the
/// Dock (top corners hold the menu bar).
fn notes_anchor(
    (_w, h): (f32, f32),
    (px, py, _pw, ph): (f32, f32, f32, f32),
    width: f32,
    height: f32,
) -> (f32, f32) {
    let below = h - (py + ph);
    if below >= height + 8.0 || px >= width + 12.0 {
        (6.0, h - 4.0)
    } else {
        // Above the Dock, which fills the bottom ~8% of the Mac's screen and
        // on a full Dock reaches nearly to the left edge.
        (px + 6.0, py + ph * (1.0 - DOCK_ZONE))
    }
}

/// Our clipboard changed: send the text to the Mac if it's a new copy. The
/// content is never logged, only its size.
fn copied_here(clipboard: &ClipboardUtil, echo: &mut ClipboardEcho, out: &UnboundedSender<String>) {
    if !clipboard.has_clipboard_text() {
        return; // an image or files: text only for now
    }
    let Ok(text) = clipboard.clipboard_text() else {
        return;
    };
    // SDL reports our own changes too; those, and repeats, stop here.
    if !echo.is_new(&text) {
        return;
    }
    if text.len() > MAX_CLIPBOARD {
        println!(
            "clipboard: skipped a copy of {} bytes (over {MAX_CLIPBOARD} bytes)",
            text.len()
        );
        return;
    }
    println!("clipboard → Mac ({} bytes)", text.len());
    let _ = out.send(text);
}

/// The Mac copied `text`: put it on our clipboard, so it pastes here.
fn copied_on_mac(clipboard: &ClipboardUtil, echo: &mut ClipboardEcho, text: &str) {
    if text.contains('\0') {
        println!("clipboard: skipped text from the Mac with a NUL byte in it");
        return;
    }
    echo.noted(text);
    match clipboard.set_clipboard_text(text) {
        Ok(()) => println!("clipboard ← Mac ({} bytes)", text.len()),
        Err(e) => eprintln!("clipboard: {e}"),
    }
}

/// The window's state as one line of JSON, for the test hooks.
#[cfg(feature = "test-hooks")]
fn state_json(
    state: &State,
    grabbed: bool,
    at_login: bool,
    overlay: &Overlay,
    (w, h): (u32, u32),
    pointer_local: bool,
) -> String {
    use crate::testhooks::json_str;
    let list = |v: &[String]| v.iter().map(|s| json_str(s)).collect::<Vec<_>>().join(",");
    let state = match state {
        State::Connected => "connected",
        State::Reconnecting { .. } => "reconnecting",
        State::GaveUp { .. } => "gave_up",
    };
    format!(
        "{{\"state\":\"{state}\",\"grabbed\":{grabbed},\"login_window\":{at_login},\
         \"dim\":{},\"message\":[{}],\"notes\":[{}],\"window\":[{w},{h}],\"pointer\":\"{}\"}}",
        overlay.dim,
        list(&overlay.message),
        list(&overlay.notes),
        if pointer_local { "local" } else { "video" }
    )
}

/// The debug font is ASCII only.
fn ascii(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Long reasons get cut; the full text is on the terminal.
    if s.len() > 90 {
        format!("{}...", &s[..87])
    } else {
        s
    }
}

fn set_title(
    canvas: &mut WindowCanvas,
    base: &str,
    fps: f64,
    grabbed: bool,
    at_login: bool,
    state: &State,
) {
    let status = match state {
        State::Reconnecting { attempt, .. } => format!("reconnecting (attempt {attempt})"),
        State::GaveUp { .. } => "disconnected (Enter to retry)".into(),
        State::Connected if at_login => {
            "LOGIN WINDOW, view only: log in at the Mac or with Screen Sharing".into()
        }
        State::Connected if grabbed => "input → Mac (Right Ctrl releases)".into(),
        State::Connected => "input LOCAL (Right Ctrl or click to grab)".into(),
    };
    let _ = canvas
        .window_mut()
        .set_title(&format!("{base} — {fps:.0} fps — {status}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_follows_state_and_grab() {
        let connected = Overlay::new(&State::Connected, true, false, false);
        assert!(connected.notes.is_empty() && !connected.dim);
        let local = Overlay::new(&State::Connected, false, false, false);
        assert!(local.notes[0].contains("LOCAL"));
        let toast = Overlay::new(&State::Connected, true, true, false);
        assert!(toast.notes[0].contains("-> Mac"));
        let lost = Overlay::new(
            &State::Reconnecting {
                attempt: 2,
                reason: "no answer".into(),
            },
            false,
            false,
            false,
        );
        assert!(lost.dim && lost.message[0].contains("attempt 2"));
        let gave_up = Overlay::new(&State::GaveUp { reason: "x".into() }, false, false, false);
        assert!(gave_up.message.iter().any(|l| l.contains("Enter to retry")));
    }

    #[test]
    fn scale_fits_the_window() {
        assert_eq!(scale_for(1259.0, 678.0, 208.0), 2.0); // tiled, the user's case
        assert_eq!(scale_for(2560.0, 1440.0, 208.0), 3.0); // fullscreen
        assert_eq!(scale_for(300.0, 678.0, 208.0), 1.0); // narrow: width wins
    }

    #[test]
    fn notes_avoid_the_picture_when_there_is_room() {
        // The Mac's picture in a wide window: bars left and right, wide
        // enough for the notes.
        let window = (1600.0, 678.0);
        let pic = picture_rect(window, (3456, 2234));
        assert!(pic.0 > 228.0);
        assert_eq!(notes_anchor(window, pic, 216.0, 18.0), (6.0, 674.0));
        // The user's tiled window: bars of ~105 px, too narrow, so the notes
        // go into the picture's own bottom-left corner.
        let window = (1259.0, 678.0);
        let pic = picture_rect(window, (3456, 2234));
        let (x, bottom) = notes_anchor(window, pic, 216.0, 18.0);
        assert!(x > pic.0);
        assert!(bottom <= pic.1 + pic.3 * (1.0 - DOCK_ZONE) + 0.01); // above the Dock
    }

    #[test]
    fn overlay_text_is_ascii_and_short() {
        assert_eq!(ascii("Mac → down"), "Mac - down");
        assert!(ascii(&"x".repeat(200)).len() <= 90);
    }
}
