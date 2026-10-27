//! Turns SDL keyboard and mouse events into protocol [`InputEvent`]s.
//!
//! Keys go out as USB HID usages (SDL scancodes are those numbers), so the
//! host maps physical keys and the Mac's own layout picks the characters.
//! Mouse positions are normalized over the remote display, so they're right
//! whatever the window size and letterboxing.

use aetherdesk_proto::{InputEvent, MouseButton};
use sdl3::event::{Event, WindowEvent};
use sdl3::mouse::{MouseButton as SdlButton, MouseWheelDirection};
use sdl3::render::WindowCanvas;
use tokio::sync::mpsc::UnboundedSender;

pub struct Forwarder {
    out: UnboundedSender<InputEvent>,
    /// HID usages we sent a key-down for and no key-up yet.
    held: Vec<u16>,
}

impl Forwarder {
    pub fn new(out: UnboundedSender<InputEvent>) -> Self {
        Self {
            out,
            held: Vec::new(),
        }
    }

    fn send(&self, event: InputEvent) {
        let _ = self.out.send(event);
    }

    /// Handles one SDL event. `remote` is the remote display size, which is
    /// also the renderer's logical size.
    pub fn handle(&mut self, event: &Event, canvas: &WindowCanvas, remote: (u32, u32)) {
        match *event {
            Event::KeyDown {
                scancode: Some(sc), ..
            } => {
                let hid = sc.to_i32() as u16;
                if !self.held.contains(&hid) {
                    self.held.push(hid);
                }
                // Repeats too: macOS doesn't auto-repeat injected keys.
                self.send(InputEvent::Key { hid, down: true });
            }
            Event::KeyUp {
                scancode: Some(sc), ..
            } => {
                let hid = sc.to_i32() as u16;
                self.held.retain(|&k| k != hid);
                self.send(InputEvent::Key { hid, down: false });
            }
            Event::MouseMotion { x, y, .. } => {
                if let Some((x, y)) = to_remote(canvas, remote, x, y) {
                    self.send(InputEvent::MouseMove { x, y });
                }
            }
            Event::MouseButtonDown {
                mouse_btn, x, y, ..
            }
            | Event::MouseButtonUp {
                mouse_btn, x, y, ..
            } => {
                let Some(button) = button(mouse_btn) else {
                    return;
                };
                // Make sure the click lands where the pointer is.
                if let Some((x, y)) = to_remote(canvas, remote, x, y) {
                    self.send(InputEvent::MouseMove { x, y });
                }
                let down = matches!(event, Event::MouseButtonDown { .. });
                self.send(InputEvent::MouseButton { button, down });
            }
            Event::MouseWheel {
                x, y, direction, ..
            } => {
                let sign = if direction == MouseWheelDirection::Flipped {
                    -1.0
                } else {
                    1.0
                };
                self.send(InputEvent::Scroll {
                    dx: x * sign,
                    dy: y * sign,
                });
            }
            Event::Window {
                win_event: WindowEvent::FocusLost,
                ..
            } => self.release_keys(),
            _ => {}
        }
    }

    /// Lets go of every held key, e.g. when the window loses focus: the
    /// key-ups then go to another window and the Mac would never see them.
    pub fn release_keys(&mut self) {
        for hid in std::mem::take(&mut self.held) {
            self.send(InputEvent::Key { hid, down: false });
        }
    }
}

fn button(b: SdlButton) -> Option<MouseButton> {
    match b {
        SdlButton::Left => Some(MouseButton::Left),
        SdlButton::Right => Some(MouseButton::Right),
        SdlButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// Window coordinates → `0.0..=1.0` over the remote display, through the
/// renderer's letterboxing. Outside the picture clamps to its edge.
fn to_remote(canvas: &WindowCanvas, remote: (u32, u32), x: f32, y: f32) -> Option<(f32, f32)> {
    let (mut lx, mut ly) = (0.0f32, 0.0f32);
    let ok = unsafe {
        sdl3::sys::render::SDL_RenderCoordinatesFromWindow(canvas.raw(), x, y, &mut lx, &mut ly)
    };
    if !ok || remote.0 == 0 || remote.1 == 0 {
        return None;
    }
    Some(normalize(lx, ly, remote))
}

fn normalize(lx: f32, ly: f32, remote: (u32, u32)) -> (f32, f32) {
    (
        (lx / remote.0 as f32).clamp(0.0, 1.0),
        (ly / remote.1 as f32).clamp(0.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_scales_and_clamps() {
        assert_eq!(normalize(1728.0, 558.5, (3456, 2234)), (0.5, 0.25));
        assert_eq!(normalize(-10.0, 5000.0, (3456, 2234)), (0.0, 1.0));
    }

    #[test]
    fn focus_loss_releases_held_keys() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut f = Forwarder::new(tx);
        f.held = vec![0xE3, 0x2B];
        f.release_keys();
        assert_eq!(
            rx.try_recv().unwrap(),
            InputEvent::Key {
                hid: 0xE3,
                down: false
            }
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            InputEvent::Key {
                hid: 0x2B,
                down: false
            }
        );
        assert!(rx.try_recv().is_err());
        f.release_keys();
        assert!(rx.try_recv().is_err(), "nothing left to release");
    }
}
