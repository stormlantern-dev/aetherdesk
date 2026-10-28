//! Locking the screen, from the agent in the user's session.
//!
//! First choice is `SACLockScreenImmediate` from the private `login`
//! framework: what the Lock Screen menu item does, no matter the user's
//! settings. If it isn't there (a future macOS), Ctrl+Cmd+Q, the documented
//! shortcut, posted like client input.

use std::ffi::c_void;

use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};

const LOGIN_FRAMEWORK: &std::ffi::CStr =
    c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";

/// `kVK_ANSI_Q`.
const KEY_Q: u16 = 12;

/// Locks the screen; returns how.
pub fn lock_screen() -> &'static str {
    if let Some(lock) = sac_lock() {
        unsafe { lock() };
        return "SACLockScreenImmediate";
    }
    shortcut();
    "Ctrl+Cmd+Q"
}

fn sac_lock() -> Option<unsafe extern "C" fn() -> i32> {
    let handle = unsafe { libc::dlopen(LOGIN_FRAMEWORK.as_ptr(), libc::RTLD_LAZY) };
    if handle.is_null() {
        return None;
    }
    let sym: *mut c_void = unsafe { libc::dlsym(handle, c"SACLockScreenImmediate".as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // The handle stays open: the framework is part of the system anyway.
    Some(unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> i32>(sym) })
}

fn shortcut() {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let flags = CGEventFlags::MaskControl.0 | CGEventFlags::MaskCommand.0;
    for down in [true, false] {
        if let Some(event) = CGEvent::new_keyboard_event(source.as_deref(), KEY_Q, down) {
            CGEvent::set_flags(Some(&event), CGEventFlags(flags));
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }
}
