//! The SDL3 window: shows the newest decoded picture, letterboxed to the
//! remote display's aspect ratio.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use aetherdesk_proto::{InputEvent, Welcome};
use anyhow::{Context, anyhow};
use sdl3::event::Event;
use sdl3::pixels::{Color, PixelFormat};
use sdl3::sys::render::SDL_RendererLogicalPresentation;

use tokio::sync::mpsc::UnboundedSender;

use crate::decoder::Picture;
use crate::input::Forwarder;
use crate::latency::{ClockSync, Stats, now_us};

pub fn run(
    welcome: &Welcome,
    host: &str,
    pictures: Receiver<Picture>,
    input: UnboundedSender<InputEvent>,
    sync: &ClockSync,
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
    canvas.set_draw_color(Color::RGB(0, 0, 0));
    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_streaming(PixelFormat::IYUV, w, h)
        .context("creating texture")?;
    let mut size = (w, h);
    let mut events = sdl.event_pump().map_err(|e| anyhow!("SDL events: {e}"))?;
    let mut forwarder = Forwarder::new(input);
    let mouse = sdl.mouse();
    // Grabbed: compositor shortcuts (Super+…) go to the Mac, and our pointer
    // is hidden because the Mac draws its own into the video. SDL shows the
    // pointer again outside the window.
    let apply_grab = |canvas: &mut sdl3::render::WindowCanvas, grabbed: bool| {
        canvas.window_mut().set_keyboard_grab(grabbed);
        mouse.show_cursor(!grabbed);
    };
    apply_grab(&mut canvas, true);
    let mut fps = 0.0;
    set_title(&mut canvas, &title, fps, true);

    let mut latency = Stats::default();
    let mut shown = 0u32;
    let mut second = Instant::now();
    loop {
        for event in events.poll_iter() {
            if let Event::Quit { .. } = event {
                forwarder.release_keys();
                return Ok(());
            }
            if let Some(grabbed) = forwarder.handle(&event, &canvas, size) {
                apply_grab(&mut canvas, grabbed);
                set_title(&mut canvas, &title, fps, grabbed);
            }
        }
        let mut picture = match pictures.recv_timeout(Duration::from_millis(10)) {
            Ok(p) => p,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                println!("connection closed");
                return Ok(());
            }
        };
        while let Ok(newer) = pictures.try_recv() {
            picture = newer;
        }
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
        canvas.clear();
        canvas
            .copy(&texture, None, None)
            .map_err(|e| anyhow!("drawing: {e}"))?;
        canvas.present();
        latency.shown(
            crate::latency::Timing {
                presented: now_us(),
                ..picture.timing
            },
            sync,
        );

        shown += 1;
        if second.elapsed() >= Duration::from_secs(1) {
            fps = f64::from(shown) / second.elapsed().as_secs_f64();
            set_title(&mut canvas, &title, fps, forwarder.grabbed());
            println!("  {fps:.0} fps shown");
            shown = 0;
            second = Instant::now();
        }
    }
}

fn set_title(canvas: &mut sdl3::render::WindowCanvas, base: &str, fps: f64, grabbed: bool) {
    let state = if grabbed {
        "input → Mac (Right Ctrl releases)"
    } else {
        "input LOCAL (Right Ctrl or click to grab)"
    };
    let _ = canvas
        .window_mut()
        .set_title(&format!("{base} — {fps:.0} fps — {state}"));
}
