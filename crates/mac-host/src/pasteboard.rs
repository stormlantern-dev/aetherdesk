//! The Mac's clipboard (the general pasteboard) during a session: text
//! copied on the Mac goes to the client, text from the client is put on it.
//!
//! Runs where the pasteboard belongs: in the user's session (the agent, or
//! `serve`), never at the login window. Only changes made during the
//! session are sent; what was on the clipboard before stays on the Mac.
//! The content is never logged, only its size.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use aetherdesk_proto::{ClipboardEcho, MAX_CLIPBOARD};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// How often the pasteboard is checked for a new copy. macOS has no change
/// notification; reading `changeCount` is cheap.
const POLL: Duration = Duration::from_millis(250);

/// A running watcher. Dropping it stops the thread.
pub struct Watcher {
    set: Sender<String>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching; `copied` gets every text copied on the Mac from now
    /// on, up to [`MAX_CLIPBOARD`] bytes.
    pub fn start(copied: impl Fn(String) + Send + 'static) -> Self {
        let (set, requests) = channel();
        let thread = std::thread::Builder::new()
            .name("pasteboard".into())
            .spawn(move || watch(requests, copied))
            .ok();
        Self { set, thread }
    }

    /// Puts text from the client on the Mac's clipboard.
    pub fn set(&self, text: String) {
        let _ = self.set.send(text);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Closing the channel ends the thread at its next poll.
        let (dead, _) = channel();
        self.set = dead;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn watch(requests: Receiver<String>, copied: impl Fn(String)) {
    let board = NSPasteboard::generalPasteboard();
    let mut echo = ClipboardEcho::default();
    let mut seen = board.changeCount();
    loop {
        match requests.recv_timeout(POLL) {
            Ok(text) => {
                echo.noted(&text);
                autoreleasepool(|_| {
                    board.clearContents();
                    let ok = board.setString_forType(&NSString::from_str(&text), unsafe {
                        NSPasteboardTypeString
                    });
                    if ok {
                        println!("  clipboard ← client ({} bytes)", text.len());
                    } else {
                        eprintln!("  clipboard: macOS refused text from the client");
                    }
                });
                // Our own write: don't read it back as a copy.
                seen = board.changeCount();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = board.changeCount();
        if now == seen {
            continue;
        }
        seen = now;
        let text = autoreleasepool(|_| {
            board
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|s| s.to_string())
        });
        // Not text (an image, a file): nothing to send.
        let Some(text) = text else { continue };
        if text.len() > MAX_CLIPBOARD {
            println!(
                "  clipboard: skipped a copy of {} bytes (over {} bytes)",
                text.len(),
                MAX_CLIPBOARD
            );
            continue;
        }
        if echo.is_new(&text) {
            println!("  clipboard → client ({} bytes)", text.len());
            copied(text);
        }
    }
}
