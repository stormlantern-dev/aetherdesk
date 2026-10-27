//! Turns SDL keyboard and mouse events into protocol [`InputEvent`]s, and
//! owns the grab state.
//!
//! Keys go out as USB HID usages (SDL scancodes are those numbers), so the
//! host maps physical keys and the Mac's own layout picks the characters.
//! Mouse positions are normalized over the remote display, so they're right
//! whatever the window size and letterboxing.
//!
//! **Grabbed** (the default): keyboard and mouse go to the Mac, and the
//! window inhibits compositor shortcuts, so Super+Q and friends reach the
//! Mac too. **Released**: nothing is forwarded; input stays local. Right
//! Ctrl toggles between the two and is never forwarded itself; clicking into
//! a released window grabs again.

use aetherdesk_proto::{InputEvent, MouseButton};
use sdl3::event::{Event, WindowEvent};
use sdl3::mouse::{MouseButton as SdlButton, MouseWheelDirection};
use sdl3::render::WindowCanvas;
use tokio::sync::mpsc::UnboundedSender;

/// HID usage of Right Ctrl, the key that hands input back to the local machine.
pub const ESCAPE_KEY: u16 = 0xE4;

pub struct Forwarder {
    out: UnboundedSender<InputEvent>,
    /// HID usages we sent a key-down for and no key-up yet.
    held: Vec<u16>,
    grabbed: bool,
    /// Right Ctrl is down: its auto-repeats must not toggle again.
    escape_down: bool,
}

impl Forwarder {
    pub fn new(out: UnboundedSender<InputEvent>) -> Self {
        Self {
            out,
            held: Vec::new(),
            grabbed: true,
            escape_down: false,
        }
    }

    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    fn send(&self, event: InputEvent) {
        let _ = self.out.send(event);
    }

    /// Handles one SDL event. `remote` is the remote display size, which is
    /// also the renderer's logical size. Returns the new grab state when it
    /// changed, for the window to apply.
    pub fn handle(
        &mut self,
        event: &Event,
        canvas: &WindowCanvas,
        remote: (u32, u32),
    ) -> Option<bool> {
        match *event {
            Event::KeyDown {
                scancode: Some(sc), ..
            } => self.key(sc.to_i32() as u16, true),
            Event::KeyUp {
                scancode: Some(sc), ..
            } => self.key(sc.to_i32() as u16, false),
            Event::MouseMotion { x, y, .. } => {
                if self.grabbed
                    && let Some((x, y)) = to_remote(canvas, remote, x, y)
                {
                    self.send(InputEvent::MouseMove { x, y });
                }
                None
            }
            Event::MouseButtonDown {
                mouse_btn, x, y, ..
            }
            | Event::MouseButtonUp {
                mouse_btn, x, y, ..
            } => {
                let down = matches!(event, Event::MouseButtonDown { .. });
                self.button(mouse_btn, down, to_remote(canvas, remote, x, y))
            }
            Event::MouseWheel {
                x, y, direction, ..
            } => {
                if self.grabbed {
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
                None
            }
            Event::Window {
                win_event: WindowEvent::FocusLost,
                ..
            } => {
                self.release_keys();
                self.escape_down = false;
                None
            }
            _ => None,
        }
    }

    fn key(&mut self, hid: u16, down: bool) -> Option<bool> {
        if hid == ESCAPE_KEY {
            let pressed = down && !self.escape_down;
            self.escape_down = down;
            return pressed.then(|| self.set_grabbed(!self.grabbed));
        }
        if !self.grabbed {
            return None;
        }
        if down {
            if !self.held.contains(&hid) {
                self.held.push(hid);
            }
        } else {
            self.held.retain(|&k| k != hid);
        }
        // Repeats too: macOS doesn't auto-repeat injected keys.
        self.send(InputEvent::Key { hid, down });
        None
    }

    fn button(&mut self, button: SdlButton, down: bool, at: Option<(f32, f32)>) -> Option<bool> {
        if !self.grabbed {
            // A click into a released window grabs it; the click stays local.
            return down.then(|| self.set_grabbed(true));
        }
        let button = map_button(button)?;
        // Make sure the click lands where the pointer is.
        if let Some((x, y)) = at {
            self.send(InputEvent::MouseMove { x, y });
        }
        self.send(InputEvent::MouseButton { button, down });
        None
    }

    fn set_grabbed(&mut self, grabbed: bool) -> bool {
        if !grabbed {
            self.release_keys();
        }
        self.grabbed = grabbed;
        grabbed
    }

    /// Lets go of every held key, e.g. when the window loses focus: the
    /// key-ups then go to another window and the Mac would never see them.
    pub fn release_keys(&mut self) {
        for hid in std::mem::take(&mut self.held) {
            self.send(InputEvent::Key { hid, down: false });
        }
    }
}

fn map_button(b: SdlButton) -> Option<MouseButton> {
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
    use tokio::sync::mpsc::UnboundedReceiver;

    fn forwarder() -> (Forwarder, UnboundedReceiver<InputEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Forwarder::new(tx), rx)
    }

    fn sent(rx: &mut UnboundedReceiver<InputEvent>) -> Vec<InputEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn key(hid: u16, down: bool) -> InputEvent {
        InputEvent::Key { hid, down }
    }

    #[test]
    fn normalize_scales_and_clamps() {
        assert_eq!(normalize(1728.0, 558.5, (3456, 2234)), (0.5, 0.25));
        assert_eq!(normalize(-10.0, 5000.0, (3456, 2234)), (0.0, 1.0));
    }

    #[test]
    fn starts_grabbed_and_forwards_keys() {
        let (mut f, mut rx) = forwarder();
        assert!(f.grabbed());
        assert_eq!(f.key(0xE3, true), None);
        assert_eq!(f.key(0xE3, false), None);
        assert_eq!(sent(&mut rx), vec![key(0xE3, true), key(0xE3, false)]);
    }

    #[test]
    fn right_ctrl_toggles_and_is_never_forwarded() {
        let (mut f, mut rx) = forwarder();
        assert_eq!(f.key(ESCAPE_KEY, true), Some(false));
        assert_eq!(f.key(ESCAPE_KEY, true), None, "auto-repeat doesn't toggle");
        assert_eq!(f.key(ESCAPE_KEY, false), None);
        assert_eq!(f.key(ESCAPE_KEY, true), Some(true));
        assert_eq!(f.key(ESCAPE_KEY, false), None);
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn released_forwards_nothing() {
        let (mut f, mut rx) = forwarder();
        f.key(ESCAPE_KEY, true);
        f.key(0x04, true);
        f.key(0x04, false);
        f.button(SdlButton::Left, false, Some((0.5, 0.5)));
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn releasing_lets_go_of_held_keys() {
        let (mut f, mut rx) = forwarder();
        f.key(0xE3, true); // Super held when the user presses Right Ctrl
        assert_eq!(f.key(ESCAPE_KEY, true), Some(false));
        assert_eq!(sent(&mut rx), vec![key(0xE3, true), key(0xE3, false)]);
    }

    #[test]
    fn click_into_released_window_grabs_without_clicking_the_mac() {
        let (mut f, mut rx) = forwarder();
        f.key(ESCAPE_KEY, true);
        f.key(ESCAPE_KEY, false);
        assert_eq!(
            f.button(SdlButton::Left, true, Some((0.5, 0.5))),
            Some(true)
        );
        assert!(sent(&mut rx).is_empty());
        // The matching button-up arrives grabbed; the Mac never saw the down,
        // and a lone up is harmless.
        f.button(SdlButton::Left, false, Some((0.5, 0.5)));
        assert_eq!(
            sent(&mut rx),
            vec![
                InputEvent::MouseMove { x: 0.5, y: 0.5 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: false
                }
            ]
        );
    }

    #[test]
    fn focus_loss_releases_held_keys() {
        let (mut f, mut rx) = forwarder();
        f.held = vec![0xE3, 0x2B];
        f.release_keys();
        assert_eq!(sent(&mut rx), vec![key(0xE3, false), key(0x2B, false)]);
        f.release_keys();
        assert!(sent(&mut rx).is_empty(), "nothing left to release");
    }
}
