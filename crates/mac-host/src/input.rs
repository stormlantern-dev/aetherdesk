//! Turns client [`InputEvent`]s into the concrete actions macOS needs:
//! virtual key codes with the right modifier flags, mouse positions in the
//! display's point space, drag vs. move, click counts for double-clicks, and
//! whole scroll lines. No platform code, so it's tested on Linux; `inject`
//! carries the actions out with `CGEventPost`.

use std::time::{Duration, Instant};

use aetherdesk_proto::{InputEvent, MouseButton};

use crate::keymap::{self, CAPS_LOCK};

// CGEventFlags bits.
pub const FLAG_CAPS_LOCK: u64 = 0x1_0000;
pub const FLAG_SHIFT: u64 = 0x2_0000;
pub const FLAG_CONTROL: u64 = 0x4_0000;
pub const FLAG_OPTION: u64 = 0x8_0000;
pub const FLAG_COMMAND: u64 = 0x10_0000;

/// Flag for each HID modifier bit 0–7: Ctrl, Shift, Alt, GUI, then right-hand.
const MODIFIER_FLAGS: [u64; 8] = [
    FLAG_CONTROL,
    FLAG_SHIFT,
    FLAG_OPTION,
    FLAG_COMMAND,
    FLAG_CONTROL,
    FLAG_SHIFT,
    FLAG_OPTION,
    FLAG_COMMAND,
];

/// macOS's default double-click interval.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// How far (in points) the pointer may move between clicks of a double-click.
const CLICK_SLOP: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// The captured display in global coordinates, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Key {
        keycode: u16,
        down: bool,
        flags: u64,
    },
    /// Pointer motion; a drag while `dragging` holds a button.
    Move {
        at: Point,
        dragging: Option<MouseButton>,
        flags: u64,
    },
    Button {
        button: MouseButton,
        down: bool,
        at: Point,
        /// 1 for a single click, 2 for the second click of a double-click, …
        clicks: i64,
        flags: u64,
    },
    Scroll {
        lines_y: i32,
        lines_x: i32,
        flags: u64,
    },
}

pub struct InputState {
    display: Rect,
    pos: Point,
    /// HID usages currently held, in press order.
    keys: Vec<u16>,
    modifiers: u8,
    caps_lock: bool,
    buttons: Vec<MouseButton>,
    last_click: Option<(MouseButton, Instant, Point, i64)>,
    scroll: (f32, f32),
}

impl InputState {
    pub fn new(display: Rect) -> Self {
        Self {
            display,
            pos: Point {
                x: display.x + display.width / 2.0,
                y: display.y + display.height / 2.0,
            },
            keys: Vec::new(),
            modifiers: 0,
            caps_lock: false,
            buttons: Vec::new(),
            last_click: None,
            scroll: (0.0, 0.0),
        }
    }

    pub fn flags(&self) -> u64 {
        let mut flags = 0;
        for (bit, flag) in MODIFIER_FLAGS.iter().enumerate() {
            if self.modifiers & (1 << bit) != 0 {
                flags |= flag;
            }
        }
        if self.caps_lock {
            flags |= FLAG_CAPS_LOCK;
        }
        flags
    }

    pub fn handle(&mut self, event: InputEvent, now: Instant) -> Vec<Action> {
        match event {
            InputEvent::Key { hid, down } => self.key(hid, down),
            InputEvent::MouseMove { x, y } => {
                self.pos = Point {
                    x: self.display.x + f64::from(x.clamp(0.0, 1.0)) * self.display.width,
                    y: self.display.y + f64::from(y.clamp(0.0, 1.0)) * self.display.height,
                };
                vec![Action::Move {
                    at: self.pos,
                    dragging: self.buttons.first().copied(),
                    flags: self.flags(),
                }]
            }
            InputEvent::MouseButton { button, down } => self.button(button, down, now),
            InputEvent::Scroll { dx, dy } => {
                self.scroll.0 += dx;
                self.scroll.1 += dy;
                let (lines_x, lines_y) = (self.scroll.0.trunc(), self.scroll.1.trunc());
                self.scroll.0 -= lines_x;
                self.scroll.1 -= lines_y;
                if lines_x == 0.0 && lines_y == 0.0 {
                    return vec![];
                }
                vec![Action::Scroll {
                    lines_y: lines_y as i32,
                    lines_x: lines_x as i32,
                    flags: self.flags(),
                }]
            }
        }
    }

    fn key(&mut self, hid: u16, down: bool) -> Vec<Action> {
        let Some(keycode) = keymap::keycode(hid) else {
            return vec![];
        };
        if down {
            if let Some(bit) = keymap::modifier_bit(hid) {
                self.modifiers |= 1 << bit;
            }
            if hid == CAPS_LOCK && !self.keys.contains(&hid) {
                self.caps_lock = !self.caps_lock;
            }
            if !self.keys.contains(&hid) {
                self.keys.push(hid);
            }
        } else {
            if let Some(bit) = keymap::modifier_bit(hid) {
                self.modifiers &= !(1 << bit);
            }
            self.keys.retain(|&k| k != hid);
        }
        vec![Action::Key {
            keycode,
            down,
            flags: self.flags(),
        }]
    }

    fn button(&mut self, button: MouseButton, down: bool, now: Instant) -> Vec<Action> {
        let clicks = if down {
            let clicks = match self.last_click {
                Some((b, t, at, n))
                    if b == button
                        && now.duration_since(t) <= DOUBLE_CLICK
                        && (at.x - self.pos.x).hypot(at.y - self.pos.y) <= CLICK_SLOP =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, now, self.pos, clicks));
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
            clicks
        } else {
            self.buttons.retain(|&b| b != button);
            match self.last_click {
                Some((b, _, _, n)) if b == button => n,
                _ => 1,
            }
        };
        vec![Action::Button {
            button,
            down,
            at: self.pos,
            clicks,
            flags: self.flags(),
        }]
    }

    /// Releases everything still held, e.g. when the client disconnects, so
    /// no key or button stays stuck down on the Mac. Modifiers go last.
    pub fn release_all(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        for button in std::mem::take(&mut self.buttons) {
            actions.push(Action::Button {
                button,
                down: false,
                at: self.pos,
                clicks: 1,
                flags: self.flags(),
            });
        }
        let (mods, others): (Vec<u16>, Vec<u16>) = self
            .keys
            .clone()
            .into_iter()
            .partition(|&k| keymap::modifier_bit(k).is_some());
        for hid in others.into_iter().chain(mods) {
            actions.extend(self.key(hid, false));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 1728.0,
        height: 1117.0,
    };

    fn key(hid: u16, down: bool) -> InputEvent {
        InputEvent::Key { hid, down }
    }

    #[test]
    fn command_flag_while_super_is_held() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        s.handle(key(0xE3, true), now); // Super down
        let tab = s.handle(key(0x2B, true), now);
        assert_eq!(
            tab,
            vec![Action::Key {
                keycode: 0x30,
                down: true,
                flags: FLAG_COMMAND
            }]
        );
        s.handle(key(0xE3, false), now);
        let tab_up = s.handle(key(0x2B, false), now);
        assert_eq!(
            tab_up,
            vec![Action::Key {
                keycode: 0x30,
                down: false,
                flags: 0
            }]
        );
    }

    #[test]
    fn left_and_right_shift_are_tracked_separately() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        s.handle(key(0xE1, true), now);
        s.handle(key(0xE5, true), now);
        s.handle(key(0xE1, false), now);
        assert_eq!(s.flags(), FLAG_SHIFT, "right Shift still held");
        s.handle(key(0xE5, false), now);
        assert_eq!(s.flags(), 0);
    }

    #[test]
    fn caps_lock_toggles_on_press_not_repeat() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        s.handle(key(CAPS_LOCK, true), now);
        s.handle(key(CAPS_LOCK, true), now); // key repeat
        s.handle(key(CAPS_LOCK, false), now);
        assert_eq!(s.flags(), FLAG_CAPS_LOCK);
        s.handle(key(CAPS_LOCK, true), now);
        s.handle(key(CAPS_LOCK, false), now);
        assert_eq!(s.flags(), 0);
    }

    #[test]
    fn mouse_position_is_scaled_and_clamped() {
        let mut s = InputState::new(Rect {
            x: 100.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
        });
        let a = s.handle(InputEvent::MouseMove { x: 0.5, y: 2.0 }, Instant::now());
        assert_eq!(
            a,
            vec![Action::Move {
                at: Point { x: 600.0, y: 500.0 },
                dragging: None,
                flags: 0
            }]
        );
    }

    #[test]
    fn moving_with_a_button_down_is_a_drag() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        s.handle(
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
            now,
        );
        let a = s.handle(InputEvent::MouseMove { x: 0.1, y: 0.1 }, now);
        assert!(matches!(
            a[0],
            Action::Move {
                dragging: Some(MouseButton::Left),
                ..
            }
        ));
    }

    #[test]
    fn double_click_counts_up_within_interval_and_distance() {
        let mut s = InputState::new(DISPLAY);
        let t0 = Instant::now();
        let click = |s: &mut InputState, t: Instant| {
            let down = s.handle(
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                },
                t,
            );
            s.handle(
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                },
                t,
            );
            match down[0] {
                Action::Button { clicks, .. } => clicks,
                _ => unreachable!(),
            }
        };
        assert_eq!(click(&mut s, t0), 1);
        assert_eq!(click(&mut s, t0 + Duration::from_millis(200)), 2);
        assert_eq!(click(&mut s, t0 + Duration::from_millis(350)), 3);
        assert_eq!(click(&mut s, t0 + Duration::from_secs(2)), 1, "too slow");
        s.handle(InputEvent::MouseMove { x: 0.9, y: 0.9 }, t0);
        assert_eq!(
            click(&mut s, t0 + Duration::from_millis(2100)),
            1,
            "moved away"
        );
    }

    #[test]
    fn scroll_accumulates_fractions_into_lines() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        assert!(
            s.handle(InputEvent::Scroll { dx: 0.0, dy: 0.4 }, now)
                .is_empty()
        );
        assert!(
            s.handle(InputEvent::Scroll { dx: 0.0, dy: 0.4 }, now)
                .is_empty()
        );
        let a = s.handle(InputEvent::Scroll { dx: 0.0, dy: 0.4 }, now);
        assert_eq!(
            a,
            vec![Action::Scroll {
                lines_y: 1,
                lines_x: 0,
                flags: 0
            }]
        );
        let a = s.handle(InputEvent::Scroll { dx: -2.0, dy: -3.0 }, now);
        assert_eq!(
            a,
            vec![Action::Scroll {
                lines_y: -2,
                lines_x: -2,
                flags: 0
            }]
        );
    }

    #[test]
    fn release_all_lets_go_of_everything_modifiers_last() {
        let mut s = InputState::new(DISPLAY);
        let now = Instant::now();
        s.handle(key(0xE3, true), now); // Super
        s.handle(key(0x04, true), now); // a
        s.handle(
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
            },
            now,
        );
        let released = s.release_all();
        assert_eq!(released.len(), 3);
        assert!(matches!(
            released[0],
            Action::Button {
                button: MouseButton::Right,
                down: false,
                ..
            }
        ));
        assert_eq!(
            released[1],
            Action::Key {
                keycode: 0x00,
                down: false,
                flags: FLAG_COMMAND
            }
        );
        assert_eq!(
            released[2],
            Action::Key {
                keycode: 0x37,
                down: false,
                flags: 0
            }
        );
        assert!(s.release_all().is_empty());
    }

    #[test]
    fn unmapped_keys_do_nothing() {
        let mut s = InputState::new(DISPLAY);
        assert!(s.handle(key(0x66, true), Instant::now()).is_empty());
    }
}
