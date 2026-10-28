//! Keeps the display on while a client watches. About a minute after the Mac
//! locks, the display sleeps, and ScreenCaptureKit then finds no displays at
//! all (M0, #3). IOKit power assertions are system-wide, so the root daemon
//! can hold them for whichever session is on screen.
//!
//! Two assertions:
//! - "prevent idle display sleep", the usual one for video players;
//! - "user activity", which also wakes a display that is already off, and
//!   is renewed while connected, since the lock screen has its own, shorter
//!   display timeout.
//!
//! Both are released when the [`KeepAwake`] is dropped, and the Mac goes
//! back to its normal sleep schedule.

use std::time::Duration;

use objc2_core_foundation::CFString;
use tokio::task::JoinHandle;

type AssertionId = u32;

/// `kIOReturnSuccess`.
const SUCCESS: i32 = 0;
/// `kIOPMAssertionLevelOn`.
const LEVEL_ON: u32 = 255;
/// `kIOPMUserActiveLocal`: as if someone touched the keyboard.
const USER_ACTIVE_LOCAL: u32 = 0;
/// How often the user-activity assertion is renewed.
const RENEW: Duration = Duration::from_secs(20);
/// What `pmset -g assertions` shows.
const NAME: &str = "AetherDesk: a remote client is watching the screen";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: &CFString,
        level: u32,
        name: &CFString,
        id: *mut AssertionId,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        name: &CFString,
        user_type: u32,
        id: *mut AssertionId,
    ) -> i32;
    fn IOPMAssertionRelease(id: AssertionId) -> i32;
}

/// Wakes the display and keeps it on until dropped; streaming goes on
/// without it, but only until the display sleeps.
pub fn keep_awake() -> Option<KeepAwake> {
    match KeepAwake::start() {
        Ok(k) => Some(k),
        Err(e) => {
            eprintln!("  can't keep the display awake: {e}");
            None
        }
    }
}

pub struct KeepAwake {
    no_idle_sleep: AssertionId,
    activity: AssertionId,
    renew: JoinHandle<()>,
}

impl KeepAwake {
    /// Wakes the display if it is off and keeps it on. Needs a tokio runtime
    /// for the renewal.
    pub fn start() -> Result<Self, String> {
        let name = CFString::from_str(NAME);
        let kind = CFString::from_str("PreventUserIdleDisplaySleep");
        let mut no_idle_sleep = 0;
        let r = unsafe { IOPMAssertionCreateWithName(&kind, LEVEL_ON, &name, &mut no_idle_sleep) };
        if r != SUCCESS {
            return Err(format!("IOPMAssertionCreateWithName failed: {r:#x}"));
        }
        let mut activity = 0;
        let r =
            unsafe { IOPMAssertionDeclareUserActivity(&name, USER_ACTIVE_LOCAL, &mut activity) };
        if r != SUCCESS {
            unsafe { IOPMAssertionRelease(no_idle_sleep) };
            return Err(format!("IOPMAssertionDeclareUserActivity failed: {r:#x}"));
        }
        let renew = tokio::spawn(async move {
            let mut tick = tokio::time::interval(RENEW);
            tick.tick().await;
            loop {
                tick.tick().await;
                // Passing the same id renews it instead of making a new one.
                let mut id = activity;
                let name = CFString::from_str(NAME);
                unsafe { IOPMAssertionDeclareUserActivity(&name, USER_ACTIVE_LOCAL, &mut id) };
            }
        });
        Ok(Self {
            no_idle_sleep,
            activity,
            renew,
        })
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.renew.abort();
        unsafe {
            IOPMAssertionRelease(self.activity);
            IOPMAssertionRelease(self.no_idle_sleep);
        }
    }
}
