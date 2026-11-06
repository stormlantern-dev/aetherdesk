//! The menu-bar item (#115): what AetherDesk.app does when it's opened
//! (`aether-agent menu`; launchd starts it at login). It shows whether the
//! host runs and has its two permissions, how to pair, and offers Sparkle's
//! "Check for Updates…". Sparkle installs the new `.pkg`, with an admin
//! prompt, since the daemon is root.
//!
//! Sparkle.framework isn't linked: it's loaded from the bundle at run time,
//! so the same binary works without it (a development build, a bare cargo
//! build). Updates stay off when the Info.plist has no public EdDSA key (a
//! development build) and when Homebrew installed the host: `brew upgrade`
//! does it there, and the two shouldn't fight.

use std::ffi::CStr;
use std::process::Command;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSVariableStatusItemLength,
};
use objc2_core_graphics::{CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess};
use objc2_foundation::{NSBundle, NSObject, NSString, ns_string};

use crate::install;

const SCREEN_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const ACCESSIBILITY_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// The menu items that change, refreshed every time the menu opens.
struct Items {
    host: Retained<NSMenuItem>,
    screen: Retained<NSMenuItem>,
    accessibility: Retained<NSMenuItem>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Actions has no
    // Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AetherDeskMenuActions"]
    #[ivars = Items]
    struct Actions;

    impl Actions {
        #[unsafe(method(openScreenRecording:))]
        fn open_screen_recording(&self, _sender: Option<&AnyObject>) {
            open(SCREEN_SETTINGS);
        }

        #[unsafe(method(openAccessibility:))]
        fn open_accessibility(&self, _sender: Option<&AnyObject>) {
            open(ACCESSIBILITY_SETTINGS);
        }

        #[unsafe(method(showPairing:))]
        fn show_pairing(&self, _sender: Option<&AnyObject>) {
            alert(
                self.mtm(),
                "Pair a client",
                "In Terminal, run:\n\n    sudo aether-agent pair\n\nIt shows a 6-digit code for \
                 2 minutes. On Linux, run aetherdesk pair and type the code.",
            );
        }

        #[unsafe(method(showUninstall:))]
        fn show_uninstall(&self, _sender: Option<&AnyObject>) {
            alert(
                self.mtm(),
                "Uninstall AetherDesk",
                "In Terminal, run:\n\n    aether-agent uninstall\n\nIt removes the app, the \
                 daemon and agents, the audio device, the logs and the pairings, and asks for \
                 your password.",
            );
        }
    }

    unsafe impl NSObjectProtocol for Actions {}

    unsafe impl NSMenuDelegate for Actions {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            self.refresh();
        }
    }
);

impl Actions {
    fn new(mtm: MainThreadMarker, items: Items) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(items);
        unsafe { msg_send![super(this), init] }
    }

    fn refresh(&self) {
        let items = self.ivars();
        let daemon = install::job_state(&install::daemon_job());
        let agent = install::job_state(&install::user_agent_job());
        let running = |s: &str| s.contains("state = running");
        let host = match (running(&daemon), running(&agent)) {
            (true, true) => "Host: running".to_string(),
            (false, _) => format!("Host: daemon {daemon}"),
            (true, false) => format!("Host: agent {agent}"),
        };
        items.host.setTitle(&NSString::from_str(&host));
        // This process is the same app as the agent, so it has the same
        // permissions.
        let permission = |ok: bool, what: &str| {
            NSString::from_str(&if ok {
                format!("{what}: granted")
            } else {
                format!("{what}: not granted — open Settings…")
            })
        };
        items.screen.setTitle(&permission(
            CGPreflightScreenCaptureAccess(),
            "Screen Recording",
        ));
        items
            .accessibility
            .setTitle(&permission(CGPreflightPostEventAccess(), "Accessibility"));
    }
}

fn open(url: &str) {
    if let Err(e) = Command::new("/usr/bin/open").arg(url).status() {
        eprintln!("open {url}: {e}");
    }
}

fn alert(mtm: MainThreadMarker, title: &str, text: &str) {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(text));
    alert.runModal();
}

/// A menu item; without an action it's greyed out (AppKit's autoenabling).
fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
    target: Option<&AnyObject>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            ns_string!(""),
        )
    };
    unsafe { item.setTarget(target) };
    item
}

/// Why updates are off, or Sparkle's updater controller, started (it checks
/// daily by itself, as the Info.plist says).
fn start_updater() -> Result<Retained<AnyObject>, &'static str> {
    let source = std::fs::read_to_string(install::SOURCE_FILE).unwrap_or_default();
    if source.trim() == "homebrew" {
        return Err("Updates: by Homebrew (brew upgrade)");
    }
    let main = NSBundle::mainBundle();
    if main
        .objectForInfoDictionaryKey(ns_string!("SUPublicEDKey"))
        .is_none()
    {
        return Err("Updates: off (development build)");
    }
    let frameworks = main
        .privateFrameworksPath()
        .ok_or("Updates: off (no Frameworks folder)")?;
    let path = NSString::from_str(&format!("{frameworks}/Sparkle.framework"));
    let sparkle = NSBundle::bundleWithPath(&path).ok_or("Updates: off (no Sparkle.framework)")?;
    if let Err(e) = unsafe { sparkle.loadAndReturnError() } {
        eprintln!("loading Sparkle: {e}");
        return Err("Updates: off (Sparkle didn't load)");
    }
    const CONTROLLER: &CStr = c"SPUStandardUpdaterController";
    let class = AnyClass::get(CONTROLLER).ok_or("Updates: off (Sparkle is incomplete)")?;
    let none: Option<&AnyObject> = None;
    // SAFETY: -[SPUStandardUpdaterController
    // initWithStartingUpdater:updaterDelegate:userDriverDelegate:] takes a
    // BOOL and two optional delegates, and returns the controller.
    let controller: Retained<AnyObject> = unsafe {
        let this: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![this, initWithStartingUpdater: true, updaterDelegate: none, userDriverDelegate: none]
    };
    Ok(controller)
}

/// Holds a per-user lock while it runs, so a second copy (launchd's at
/// login, plus one opened from Finder or relaunched by Sparkle) steps aside.
fn single_instance() -> Result<bool, String> {
    let mut buf = [0u8; 1024];
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let dir = if len > 0 && len <= buf.len() {
        String::from_utf8_lossy(&buf[..len - 1]).into_owned()
    } else {
        format!("/tmp/aetherdesk-{}", unsafe { libc::getuid() })
    };
    let path = std::path::Path::new(&dir).join("nl.stormlantern.aetherdesk.menu.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(false);
    }
    // Held until the process ends.
    std::mem::forget(file);
    Ok(true)
}

/// `aether-agent menu`, or AetherDesk.app opened without arguments.
pub fn run(_args: &[String]) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    if !single_instance()? {
        println!("the menu runs already");
        return Ok(());
    }
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon (the Info.plist says so too: LSUIElement).
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let updater = start_updater();
    match &updater {
        Ok(_) => println!("updater started"),
        Err(why) => println!("{why}"),
    }

    let items = Items {
        host: item(mtm, "Host: …", None, None),
        screen: item(mtm, "Screen Recording: …", None, None),
        accessibility: item(mtm, "Accessibility: …", None, None),
    };
    let menu = NSMenu::new(mtm);
    let version = format!("AetherDesk {}", env!("CARGO_PKG_VERSION"));
    menu.addItem(&item(mtm, &version, None, None));
    menu.addItem(&items.host);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let actions_items = (items.screen.clone(), items.accessibility.clone());
    let actions = Actions::new(mtm, items);
    let target: &AnyObject = &actions;
    unsafe {
        actions_items.0.setAction(Some(sel!(openScreenRecording:)));
        actions_items.0.setTarget(Some(target));
        actions_items.1.setAction(Some(sel!(openAccessibility:)));
        actions_items.1.setTarget(Some(target));
    }
    menu.addItem(&actions_items.0);
    menu.addItem(&actions_items.1);
    menu.addItem(&item(
        mtm,
        "Pair a Client…",
        Some(sel!(showPairing:)),
        Some(target),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    match &updater {
        Ok(controller) => menu.addItem(&item(
            mtm,
            "Check for Updates…",
            Some(sel!(checkForUpdates:)),
            Some(controller),
        )),
        Err(why) => menu.addItem(&item(mtm, why, None, None)),
    }
    menu.addItem(&item(
        mtm,
        "Uninstall…",
        Some(sel!(showUninstall:)),
        Some(target),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, "Quit Menu", Some(sel!(terminate:)), None));
    menu.setDelegate(Some(ProtocolObject::from_ref(&*actions)));
    actions.refresh();

    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status.button(mtm) {
        match NSImage::imageNamed(ns_string!("MenuBarIdleTemplate")) {
            Some(image) => {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            // Not in a bundle (a bare cargo build): no icon to show.
            None => button.setTitle(ns_string!("AetherDesk")),
        }
    }
    status.setMenu(Some(&menu));

    app.run();
    // terminate: exits before this; keep everything alive until then.
    drop((status, actions, updater));
    Ok(())
}
