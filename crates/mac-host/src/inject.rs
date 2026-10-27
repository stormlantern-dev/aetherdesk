//! Carries out [`Action`]s on the Mac with `CGEventPost`. Needs the
//! Accessibility permission (for whatever app started the agent, in M1).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use aetherdesk_proto::{InputEvent, MouseButton};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};

use crate::input::{Action, InputState, Point, Rect};

struct Injector {
    source: Option<CFRetained<CGEventSource>>,
}

impl Injector {
    fn new() -> Self {
        Self {
            source: CGEventSource::new(CGEventSourceStateID::HIDSystemState),
        }
    }

    fn post(&self, event: Option<CFRetained<CGEvent>>, flags: u64) {
        if let Some(event) = event {
            CGEvent::set_flags(Some(&event), CGEventFlags(flags));
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }

    fn apply(&self, action: Action) {
        let source = self.source.as_deref();
        match action {
            Action::Key {
                keycode,
                down,
                flags,
            } => {
                self.post(CGEvent::new_keyboard_event(source, keycode, down), flags);
            }
            Action::Move {
                at,
                dragging,
                flags,
            } => {
                let (kind, button) = match dragging {
                    None => (CGEventType::MouseMoved, CGMouseButton::Left),
                    Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
                    Some(MouseButton::Right) => {
                        (CGEventType::RightMouseDragged, CGMouseButton::Right)
                    }
                    Some(MouseButton::Middle) => {
                        (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                    }
                };
                self.post(
                    CGEvent::new_mouse_event(source, kind, point(at), button),
                    flags,
                );
            }
            Action::Button {
                button,
                down,
                at,
                clicks,
                flags,
            } => {
                let (kind, cg_button) = match (button, down) {
                    (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                    (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    (MouseButton::Right, true) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right)
                    }
                    (MouseButton::Right, false) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right)
                    }
                    (MouseButton::Middle, true) => {
                        (CGEventType::OtherMouseDown, CGMouseButton::Center)
                    }
                    (MouseButton::Middle, false) => {
                        (CGEventType::OtherMouseUp, CGMouseButton::Center)
                    }
                };
                let event = CGEvent::new_mouse_event(source, kind, point(at), cg_button);
                if let Some(event) = &event {
                    CGEvent::set_integer_value_field(
                        Some(event),
                        CGEventField::MouseEventClickState,
                        clicks,
                    );
                }
                self.post(event, flags);
            }
            Action::Scroll {
                lines_y,
                lines_x,
                flags,
            } => {
                let event = CGEvent::new_scroll_wheel_event2(
                    source,
                    CGScrollEventUnit::Line,
                    2,
                    lines_y,
                    lines_x,
                    0,
                );
                self.post(event, flags);
            }
        }
    }
}

fn point(p: Point) -> CGPoint {
    CGPoint { x: p.x, y: p.y }
}

/// Injects events until `events` closes, then releases whatever is still
/// held. Run it on its own thread for the length of a session.
pub fn run(display: Rect, events: Receiver<InputEvent>) {
    let injector = Injector::new();
    let mut state = InputState::new(display);
    for event in events {
        for action in state.handle(event, Instant::now()) {
            injector.apply(action);
        }
    }
    for action in state.release_all() {
        injector.apply(action);
    }
}
