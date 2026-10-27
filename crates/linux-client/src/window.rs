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

pub fn run(
    welcome: &Welcome,
    host: &str,
    pictures: Receiver<Picture>,
    input: UnboundedSender<InputEvent>,
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

    let mut shown = 0u32;
    let mut second = Instant::now();
    loop {
        for event in events.poll_iter() {
            if let Event::Quit { .. } = event {
                forwarder.release_keys();
                return Ok(());
            }
            forwarder.handle(&event, &canvas, size);
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

        shown += 1;
        if second.elapsed() >= Duration::from_secs(1) {
            let fps = f64::from(shown) / second.elapsed().as_secs_f64();
            let _ = canvas
                .window_mut()
                .set_title(&format!("{title} — {fps:.0} fps"));
            println!("  {fps:.0} fps shown");
            shown = 0;
            second = Instant::now();
        }
    }
}
