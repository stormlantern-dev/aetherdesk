//! An on-screen sign that a client is connected (#103): whoever sits at the
//! Mac sees it. On connect a coloured border goes round the screen for a few
//! seconds; then a small badge with the client's name stays in a corner for
//! the rest of the session. Both are click-through and left out of the
//! capture, so the client never sees them.
//!
//! AppKit windows need the main thread, which the agent's runtime holds, so
//! the windows live in a helper process: `auroval-agent _indicator <name>`.
//! The agent starts it per session ([`Indicator::start`]) and the capture
//! leaves out its windows ([`crate::capture::exclude_process`]). The helper
//! exits when its stdin closes, so it can't outlive the agent.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask, NSFont,
    NSFontWeightSemibold, NSScreen, NSTextField, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};

/// How long the border stays after connecting.
const BORDER_FOR: Duration = Duration::from_secs(4);
/// Border width in points.
const BORDER: f64 = 6.0;
/// Above normal windows, full-screen apps and the Dock.
const LEVEL: isize = 1000;
/// How long the agent waits for the helper's windows before capturing
/// without them.
const READY_WAIT: Duration = Duration::from_secs(3);

/// The running helper; dropping it takes the indicator down.
pub struct Indicator {
    child: Child,
}

impl Indicator {
    /// Starts the helper for `client` and waits until its windows are up,
    /// so the capture that starts next can leave them out.
    pub fn start(client: &str) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let mut child = Command::new(exe)
            .args(["_indicator", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting it: {e}"))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let indicator = Self { child };
        match rx.recv_timeout(READY_WAIT) {
            Ok(line) if line.trim() == "ready" => {
                crate::capture::exclude_process(indicator.child.id() as i32);
                Ok(indicator)
            }
            Ok(line) => Err(format!("it said {:?} instead of ready", line.trim())),
            Err(_) => Err(format!("not ready after {} s", READY_WAIT.as_secs())),
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        crate::capture::include_process(self.child.id() as i32);
        // Closing stdin ends it; kill in case it hangs.
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `auroval-agent _indicator <client name>`: shows the border and badge until
/// stdin closes.
pub fn run(args: &[String]) -> Result<(), String> {
    let name = args
        .first()
        .ok_or("usage: auroval-agent _indicator <client name>")?;
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            // The agent holds the other end; EOF means it's done or gone.
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
            stop.store(true, Ordering::Relaxed);
        });
    }

    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, no menu bar, never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let screen = NSScreen::mainScreen(mtm).ok_or("no screen")?;
    let full = screen.frame();
    let colour = accent();
    let border = border_windows(mtm, full, &colour);
    let badge = badge_window(mtm, screen.visibleFrame(), &colour, name);
    // All up before "ready", so the capture that starts next finds this
    // process on screen and leaves it out.
    for w in border.iter().chain([&badge]) {
        w.orderFrontRegardless();
    }

    let started = Instant::now();
    let mut ready = false;
    let mut border_up = true;
    while !stop.load(Ordering::Relaxed) {
        pump(&app, 0.05);
        if !ready && started.elapsed() > Duration::from_millis(150) {
            // The window server has them; the agent can start capturing.
            println!("ready");
            let _ = std::io::stdout().flush();
            ready = true;
        }
        if border_up && started.elapsed() > BORDER_FOR {
            for w in &border {
                w.orderOut(None);
            }
            border_up = false;
        }
    }
    Ok(())
}

/// Handles the window server's events for up to `seconds`.
fn pump(app: &NSApplication, seconds: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
    while let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&event);
    }
}

/// Auroval's orange: the same in light and dark mode, and readable with
/// dark text on it.
fn accent() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.62, 0.04, 1.0)
}

/// A borderless, click-through window above everything, on every Space.
fn overlay_window(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setIgnoresMouseEvents(true);
    window.setLevel(LEVEL);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setHasShadow(false);
    window
}

/// Four thin windows along the screen's edges.
fn border_windows(mtm: MainThreadMarker, s: NSRect, colour: &NSColor) -> Vec<Retained<NSWindow>> {
    let (x, y, w, h) = (s.origin.x, s.origin.y, s.size.width, s.size.height);
    [
        (x, y + h - BORDER, w, BORDER), // top
        (x, y, w, BORDER),              // bottom
        (x, y, BORDER, h),              // left
        (x + w - BORDER, y, BORDER, h), // right
    ]
    .into_iter()
    .map(|(x, y, w, h)| {
        let window = overlay_window(mtm, rect(x, y, w, h));
        window.setBackgroundColor(Some(colour));
        window
    })
    .collect()
}

/// `● Auroval: <name>`, small, in the top right corner under the menu
/// bar.
fn badge_window(
    mtm: MainThreadMarker,
    visible: NSRect,
    colour: &NSColor,
    name: &str,
) -> Retained<NSWindow> {
    const MARGIN: f64 = 8.0;
    const PAD_X: f64 = 8.0;
    const PAD_Y: f64 = 2.0;
    let label = NSTextField::labelWithString(&NSString::from_str(&text(name)), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, unsafe {
        NSFontWeightSemibold
    })));
    label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.08, 0.08, 0.1, 1.0,
    )));
    label.sizeToFit();
    let text_size = label.frame().size;
    let (w, h) = (
        text_size.width + 2.0 * PAD_X,
        text_size.height + 2.0 * PAD_Y,
    );
    let x = visible.origin.x + visible.size.width - w - MARGIN;
    let y = visible.origin.y + visible.size.height - h - MARGIN;

    let window = overlay_window(mtm, rect(x, y, w, h));
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, w, h));
    content.setWantsLayer(true);
    // Rounded, filled with the accent: set on the layer, which saves
    // pulling in Core Animation's bindings for two properties.
    unsafe {
        let layer: *mut AnyObject = msg_send![&content, layer];
        if let Some(layer) = layer.as_ref() {
            let cg = colour.CGColor();
            let _: () = msg_send![layer, setBackgroundColor: &*cg];
            let _: () = msg_send![layer, setCornerRadius: h / 2.0];
        }
    }
    label.setFrameOrigin(NSPoint::new(PAD_X, PAD_Y));
    content.addSubview(&label);
    window.setContentView(Some(&content));
    window.setHasShadow(true);
    window.setAlphaValue(0.9);
    window
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// The badge's text. Colour plus words: never colour alone.
fn text(name: &str) -> String {
    format!("● Auroval: {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_names_the_client() {
        assert_eq!(text("werkkamer"), "● Auroval: werkkamer");
    }
}
