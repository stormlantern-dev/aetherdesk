//! Installing the host as launchd jobs: a root daemon (the network side),
//! an agent in every GUI session (login window and user) and the menu-bar
//! item in the user's session, all running one copy of the app,
//! `/Applications/AetherDesk.app` ([`crate::bundle`]). Its identity and
//! paired clients stay in `/Library/Application Support/AetherDesk`.
//!
//! Two ways in, doing the same root part:
//! - the `.pkg` installer (`packaging/macos/build-pkg.sh`): its payload puts
//!   the app in place, its postinstall runs `aether-agent _postinstall`;
//! - `aether-agent install`, from wherever the binary is (inside a Homebrew
//!   cask's or a tarball's app, or a bare cargo build, which it wraps in a
//!   bundle): it signs a copy when needed, then asks for admin rights.
//!
//! The AetherDesk audio device (#82) travels inside the app
//! (`Contents/PlugIns/AetherDesk.driver`; next to a bare binary for a
//! development build) and is installed from there: coreaudiod loads it from
//! the HAL plug-ins folder, and the daemon's job declares the Mach service
//! the device looks up.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;

const LABEL: &str = "nl.stormlantern.aetherdesk";
/// The host's data: identity, pairings ([`crate::trust::SYSTEM_DIR`]).
const DEST: &str = "/Library/Application Support/AetherDesk";
/// Where the binary was installed before the app bundle; removed on install.
const LEGACY_BIN: &str = "/Library/Application Support/AetherDesk/aether-agent";
/// Who installed the host: `pkg`, `homebrew` or `manual`. The menu reads it
/// to leave updating to Homebrew.
pub const SOURCE_FILE: &str = "/Library/Application Support/AetherDesk/install-source";
const LOGS: &str = "/Library/Logs/AetherDesk";
/// Where coreaudiod finds the AetherDesk audio device.
const DRIVER: &str = "/Library/Audio/Plug-Ins/HAL/AetherDesk.driver";
/// The Mach service the device looks up: [`crate::speaker`].
const SPEAKER_SERVICE: &str = "nl.stormlantern.aetherdesk.audio";
/// `aether-agent` on the PATH for installs that don't bring their own
/// (Homebrew links it itself).
const CLI_LINK: &str = "/usr/local/bin/aether-agent";
/// The installer package's identifier, forgotten on uninstall.
pub const PKG_ID: &str = "nl.stormlantern.aetherdesk.pkg";

fn daemon_plist() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.daemon.plist"))
}

/// The jobs in GUI sessions: the user's agent, the login window's agent and
/// the menu-bar item.
fn agent_plists() -> [PathBuf; 3] {
    [
        PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.agent.plist")),
        PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.loginwindow.plist")),
        PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.menu.plist")),
    ]
}

/// The user-session jobs `install` starts right away; launchd starts them by
/// itself at every login.
const USER_JOBS: [&str; 2] = ["agent", "menu"];

pub const USAGE: &str = "\
aether-agent install     install (or update) AetherDesk.app in /Applications with
                         the daemon and agents; asks for your password. A
                         release build keeps its Developer ID signature, so
                         permissions survive updates. Other builds are signed
                         with the code-signing identity $AETHERDESK_IDENTITY
                         (default \"AetherDesk Dev\") when you have one;
                         otherwise ad hoc, and macOS asks for the permissions
                         again after each update.
aether-agent restart     restart your agent, e.g. after granting it Screen
                         Recording or Accessibility (no password needed)
aether-agent uninstall   remove it all again: app, daemon, agents, audio device,
                         logs and pairings
aether-agent status      are the daemon and your agent running? last log lines";

/// Runs `program` with `args`, failing with its stderr.
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// The user at the screen, if one is logged in (not the login window).
fn console_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/dev/console").ok()?.uid();
    (uid != 0).then_some(uid)
}

/// `launchctl bootout`, then wait until launchd has let go: bootout returns
/// before the job is gone, and a bootstrap right after it fails with
/// "Bootstrap failed: 5: Input/output error".
fn unload(job: &str) {
    if run("launchctl", &["bootout", job]).is_err() {
        return; // wasn't loaded
    }
    for _ in 0..50 {
        if run("launchctl", &["print", job]).is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    eprintln!("warning: {job} is still loaded after 10 s");
}

/// (Re)starts the user-session jobs in `uid`'s GUI session.
fn start_user_jobs(uid: u32) -> Result<(), String> {
    let domain = format!("gui/{uid}");
    let [agent, _, menu] = agent_plists();
    for (job, plist) in USER_JOBS.iter().zip([agent, menu]) {
        unload(&format!("{domain}/{LABEL}.{job}"));
        run(
            "launchctl",
            &["bootstrap", &domain, &plist.to_string_lossy()],
        )?;
    }
    Ok(())
}

struct Job<'a> {
    label: &'a str,
    session_type: Option<&'a str>,
    log: &'a str,
    arg: &'a str,
    mach_service: Option<&'a str>,
    keep_alive: bool,
}

fn plist(job: &Job) -> String {
    let limit = job
        .session_type
        .map(|t| format!("  <key>LimitLoadToSessionType</key><string>{t}</string>\n"))
        .unwrap_or_default();
    let service = job
        .mach_service
        .map(|s| format!("  <key>MachServices</key><dict><key>{s}</key><true/></dict>\n"))
        .unwrap_or_default();
    let keep_alive = if job.keep_alive {
        "<true/>"
    } else {
        "<false/>"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
    <string>{bin}</string>
    <string>{arg}</string>
  </array>
  <key>AssociatedBundleIdentifiers</key><string>{id}</string>
{limit}{service}  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>{keep_alive}
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{LOGS}/{log}</string>
  <key>StandardErrorPath</key><string>{LOGS}/{log}</string>
</dict></plist>
"#,
        label = job.label,
        arg = job.arg,
        log = job.log,
        bin = bundle::BIN,
        id = bundle::BUNDLE_ID,
    )
}

/// `aether-agent install`, as the user: stages a signed copy of the app and
/// hands it to the root part through sudo.
pub fn install(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.into());
    }
    if uid() == 0 {
        return Err(
            "run it as yourself, not with sudo: it signs with your keychain's identity and \
             asks for admin rights itself"
                .into(),
        );
    }
    let me = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("this binary: {e}"))?;
    let source = bundle::install_source(&me);
    let stage = std::env::temp_dir().join(format!("aetherdesk-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let result = stage_and_install(&me, &stage, source);
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn stage_and_install(me: &Path, stage: &Path, source: &str) -> Result<(), String> {
    let app = stage.join("AetherDesk.app");
    let app_s = app.to_string_lossy().to_string();
    match bundle::enclosing_bundle(me) {
        Some(from) => run("ditto", &[&from.to_string_lossy(), &app_s])?,
        None => wrap_in_bundle(me, &app)?,
    }
    // A cask's download carries the quarantine flag; the copy shouldn't.
    let _ = run("xattr", &["-dr", "com.apple.quarantine", &app_s]);

    // A release is signed with our Developer ID and notarized: keep that,
    // since permissions stick to it across updates for everyone.
    if developer_id(&app_s) {
        println!("signed with the AetherDesk Developer ID: permissions survive updates");
    } else {
        sign_for_development(&app)?;
    }
    install_staged(&app, source)
}

/// A bare binary (a cargo build) in a minimal app, with the audio device
/// when it lies next to the binary (`scripts/mac.zsh install` builds it
/// there). `packaging/macos/app-bundle.sh` makes the full one.
fn wrap_in_bundle(me: &Path, app: &Path) -> Result<(), String> {
    let io = |what: &Path, e: std::io::Error| format!("{}: {e}", what.display());
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).map_err(|e| io(&macos, e))?;
    let bin = macos.join("aether-agent");
    std::fs::copy(me, &bin).map_err(|e| io(&bin, e))?;
    let info = bundle::info_plist(env!("CARGO_PKG_VERSION"), bundle::FEED_URL, None)?;
    let plist = app.join("Contents/Info.plist");
    std::fs::write(&plist, info).map_err(|e| io(&plist, e))?;
    let driver = me.parent().map(|d| d.join("AetherDesk.driver"));
    match driver.filter(|d| d.is_dir()) {
        Some(driver) => {
            let plugins = app.join("Contents/PlugIns");
            std::fs::create_dir_all(&plugins).map_err(|e| io(&plugins, e))?;
            run(
                "ditto",
                &[
                    &driver.to_string_lossy(),
                    &app.join(bundle::DRIVER_IN_BUNDLE).to_string_lossy(),
                ],
            )?;
        }
        None => println!(
            "no AetherDesk.driver next to {}: installing without the audio device (the Mac's \
             sound is captured instead)",
            me.display()
        ),
    }
    println!("wrapped {} in AetherDesk.app", me.display());
    Ok(())
}

/// Signs a build that isn't a release with $AETHERDESK_IDENTITY, or ad hoc.
/// One identifier for every build (the bundle's), so permissions stick to
/// it. Nested code first, the app last; the audio device keeps its own
/// signature (coreaudiod takes ad hoc).
fn sign_for_development(app: &Path) -> Result<(), String> {
    let identity = std::env::var("AETHERDESK_IDENTITY").unwrap_or_else(|_| "AetherDesk Dev".into());
    let have_identity = Command::new("security")
        .args(["find-identity", "-p", "codesigning"])
        .output()
        // codesign takes the start of a name too ("Developer ID Application").
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{identity}")));
    let sign_as = if have_identity {
        identity.as_str()
    } else {
        "-"
    };
    let sparkle = app.join("Contents/Frameworks/Sparkle.framework");
    let nested = [
        sparkle.join("Versions/B/Autoupdate"),
        sparkle.join("Versions/B/Updater.app"),
        sparkle.clone(),
    ];
    for code in nested.iter().filter(|p| p.exists()) {
        run(
            "codesign",
            &["--force", "--sign", sign_as, &code.to_string_lossy()],
        )?;
    }
    run(
        "codesign",
        &["--force", "--sign", sign_as, &app.to_string_lossy()],
    )?;
    if have_identity {
        println!("signed with \"{identity}\": permissions survive updates");
    } else {
        println!(
            "signed ad hoc (no \"{identity}\" code-signing identity): macOS will ask for the \
             permissions again after each update. To avoid that, create one in Keychain Access \
             → Certificate Assistant → Create a Certificate (type: Code Signing)."
        );
    }
    Ok(())
}

/// Whether `path` carries a valid Developer ID Application signature.
fn developer_id(path: &str) -> bool {
    let valid = Command::new("codesign")
        .args(["--verify", "--deep", "--strict", path])
        .output()
        .is_ok_and(|o| o.status.success());
    valid
        && Command::new("codesign")
            .args(["-dv", "--verbose=2", path])
            .output()
            .is_ok_and(|o| signed_by_developer_id(&String::from_utf8_lossy(&o.stderr)))
}

/// Reads `codesign -dv --verbose=2` output (which goes to stderr).
fn signed_by_developer_id(info: &str) -> bool {
    info.lines()
        .any(|l| l.starts_with("Authority=Developer ID Application:"))
}

/// Hands the staged app to the root part through sudo, then starts the
/// user's agent and menu.
fn install_staged(app: &Path, source: &str) -> Result<(), String> {
    println!("Installing needs admin rights; macOS asks for your password.");
    let status = Command::new("sudo")
        .arg(app.join("Contents/MacOS/aether-agent"))
        .args(["_install-root", &app.to_string_lossy(), source])
        .status()
        .map_err(|e| format!("sudo: {e}"))?;
    if !status.success() {
        return Err("the admin part didn't finish".into());
    }
    start_user_jobs(uid())?;
    println!(
        "\nInstalled {}. Once, grant AetherDesk both Screen Recording and Accessibility in \
         System Settings → Privacy & Security (+ button, then pick AetherDesk in \
         Applications), then run: aether-agent restart\nTo let a client in: sudo aether-agent pair",
        bundle::APP
    );
    Ok(())
}

/// The root part of `install`: puts the staged app in place, then does what
/// the installer's postinstall does.
pub fn install_root(args: &[String]) -> Result<(), String> {
    let [staged, source] = args else {
        return Err("_install-root needs the staged app and the install source".into());
    };
    if uid() != 0 {
        return Err("_install-root runs as root, from install".into());
    }
    unload(&format!("system/{LABEL}.daemon"));
    // A new inode for every file: overwriting a signed binary in place can
    // get it killed.
    let _ = std::fs::remove_dir_all(bundle::APP);
    run("ditto", &[staged, bundle::APP])?;
    run("chown", &["-R", "root:wheel", bundle::APP])?;
    run("chmod", &["-R", "go-w", bundle::APP])?;
    setup_root(source)
}

/// `aether-agent _postinstall`, from the installer package: the payload has
/// put the app in place; set up the rest and start the console user's jobs.
pub fn postinstall(_args: &[String]) -> Result<(), String> {
    if uid() != 0 {
        return Err("_postinstall runs as root, from the installer".into());
    }
    setup_root("pkg")?;
    match console_uid() {
        Some(user) => {
            if let Err(e) = start_user_jobs(user) {
                // launchd starts them at the next login anyway.
                eprintln!("warning: starting the user's agent: {e}");
            }
        }
        None => println!("nobody logged in: the agents start at the next login"),
    }
    Ok(())
}

/// What both installs do as root once the app is in place: the data and log
/// folders, the launchd jobs, the audio device, then start the daemon.
fn setup_root(source: &str) -> Result<(), String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    unload(&format!("system/{LABEL}.daemon"));
    std::fs::create_dir_all(DEST).map_err(|e| io(DEST, e))?;
    std::fs::create_dir_all(LOGS).map_err(|e| io(LOGS, e))?;
    // The user's agent writes its log here too.
    run("chmod", &["1777", LOGS])?;
    // Before the app bundle, the binary itself lived here; M2's shared
    // token was replaced by pairing.
    let _ = std::fs::remove_file(LEGACY_BIN);
    let _ = std::fs::remove_file(Path::new(DEST).join("token"));
    std::fs::write(SOURCE_FILE, format!("{source}\n")).map_err(|e| io(SOURCE_FILE, e))?;
    run("chmod", &["644", SOURCE_FILE])?;

    let [agent, loginwindow, menu] = agent_plists();
    let jobs = [
        (
            daemon_plist(),
            Job {
                label: &format!("{LABEL}.daemon"),
                session_type: None,
                log: "daemon.log",
                arg: "daemon",
                mach_service: Some(SPEAKER_SERVICE),
                keep_alive: true,
            },
        ),
        (
            agent,
            Job {
                label: &format!("{LABEL}.agent"),
                session_type: Some("Aqua"),
                log: "agent.log",
                arg: "agent",
                mach_service: None,
                keep_alive: true,
            },
        ),
        (
            loginwindow,
            Job {
                label: &format!("{LABEL}.loginwindow"),
                session_type: Some("LoginWindow"),
                log: "loginwindow.log",
                arg: "agent",
                mach_service: None,
                keep_alive: true,
            },
        ),
        (
            menu,
            // Quitting it from its menu keeps it gone until the next login.
            Job {
                label: &format!("{LABEL}.menu"),
                session_type: Some("Aqua"),
                log: "menu.log",
                arg: "menu",
                mach_service: None,
                keep_alive: false,
            },
        ),
    ];
    for (path, job) in &jobs {
        std::fs::write(path, plist(job)).map_err(|e| io(&path.to_string_lossy(), e))?;
        let p = path.to_string_lossy();
        run("chown", &["root:wheel", &p])?;
        run("chmod", &["644", &p])?;
    }
    if source != "homebrew" {
        link_cli();
    }
    let driver = Path::new(bundle::APP).join(bundle::DRIVER_IN_BUNDLE);
    if driver.is_dir() {
        install_driver(&driver)?;
    }
    run(
        "launchctl",
        &["bootstrap", "system", &daemon_plist().to_string_lossy()],
    )
}

/// `aether-agent` on the PATH, unless something else is there already.
fn link_cli() {
    let link = Path::new(CLI_LINK);
    match std::fs::symlink_metadata(link) {
        Ok(m) if !m.file_type().is_symlink() => {
            eprintln!("warning: {CLI_LINK} exists and isn't a link; leaving it");
            return;
        }
        Ok(_) => {
            let _ = std::fs::remove_file(link);
        }
        Err(_) => {}
    }
    let made = link
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::os::unix::fs::symlink(bundle::BIN, link));
    if let Err(e) = made {
        eprintln!("warning: linking {CLI_LINK}: {e}");
    }
}

/// Puts the audio device in place and restarts coreaudiod to load it; not
/// when it's unchanged, since the restart cuts the Mac's sound for a moment.
fn install_driver(staged: &Path) -> Result<(), String> {
    let files = ["Contents/MacOS/AetherDesk", "Contents/Info.plist"];
    let unchanged = files.iter().all(|f| {
        let new = std::fs::read(staged.join(f));
        new.is_ok() && new.ok() == std::fs::read(Path::new(DRIVER).join(f)).ok()
    });
    if unchanged {
        println!("audio device: unchanged");
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(DRIVER);
    run("ditto", &[&staged.to_string_lossy(), DRIVER])?;
    run("chown", &["-R", "root:wheel", DRIVER])?;
    restart_coreaudio();
    println!("audio device installed (the Mac's sound paused for a moment)");
    Ok(())
}

/// coreaudiod loads and drops plug-ins only when it starts; launchd starts
/// it again right away.
fn restart_coreaudio() {
    if let Err(e) = run("killall", &["coreaudiod"]) {
        eprintln!("warning: restarting coreaudiod: {e}");
    }
}

/// `aether-agent uninstall`, as the user.
pub fn uninstall(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.into());
    }
    for job in USER_JOBS {
        unload(&format!("gui/{}/{LABEL}.{job}", uid()));
    }
    println!("Uninstalling needs admin rights; macOS asks for your password.");
    let me = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let status = Command::new("sudo")
        .arg(&me)
        .arg("_uninstall-root")
        .status()
        .map_err(|e| format!("sudo: {e}"))?;
    if !status.success() {
        return Err("the admin part didn't finish".into());
    }
    println!("Removed. The AetherDesk entries in Privacy & Security stay until you remove them.");
    Ok(())
}

/// The root part of `uninstall`.
pub fn uninstall_root(_args: &[String]) -> Result<(), String> {
    if uid() != 0 {
        return Err("_uninstall-root runs as root, from uninstall".into());
    }
    unload(&format!("system/{LABEL}.daemon"));
    if let Some(user) = console_uid() {
        for job in USER_JOBS {
            unload(&format!("gui/{user}/{LABEL}.{job}"));
        }
    }
    for p in std::iter::once(daemon_plist()).chain(agent_plists()) {
        let _ = std::fs::remove_file(p);
    }
    if std::fs::read_link(CLI_LINK).is_ok_and(|t| t == Path::new(bundle::BIN)) {
        let _ = std::fs::remove_file(CLI_LINK);
    }
    for dir in [bundle::APP, DEST, LOGS, "/var/run/aetherdesk"] {
        let _ = std::fs::remove_dir_all(dir);
    }
    if Path::new(DRIVER).exists() {
        let _ = std::fs::remove_dir_all(DRIVER);
        restart_coreaudio();
    }
    // Installed by the .pkg: so the receipt doesn't claim it's still there.
    let _ = run("pkgutil", &["--forget", PKG_ID]);
    Ok(())
}

/// `aether-agent restart`: restarts the user's agent, so a permission
/// granted after it started takes effect. The login window's agent starts
/// afresh at every logout, so it picks the permissions up by itself.
pub fn restart(_args: &[String]) -> Result<(), String> {
    let job = format!("gui/{}/{LABEL}.agent", uid());
    run("launchctl", &["kickstart", "-k", &job])
        .map_err(|e| format!("{e}\nIs the host installed? Run: aether-agent install"))?;
    println!("Restarted your agent. Check it with: aether-agent status");
    Ok(())
}

/// One line per job: `state = running, pid = 123`, or `not loaded`.
pub fn job_state(job: &str) -> String {
    match Command::new("launchctl").args(["print", job]).output() {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            // Nested blocks (endpoints, sockets) have their own `state`;
            // the job's own comes first.
            let first = |key: &str| text.lines().map(str::trim).find(|l| l.starts_with(key));
            let fields: Vec<&str> = [first("state ="), first("pid =")]
                .into_iter()
                .flatten()
                .collect();
            fields.join(", ")
        }
        _ => "not loaded".into(),
    }
}

/// The launchd job of the user's agent, for [`crate::menu`].
pub fn user_agent_job() -> String {
    format!("gui/{}/{LABEL}.agent", uid())
}

/// The daemon's launchd job.
pub fn daemon_job() -> String {
    format!("system/{LABEL}.daemon")
}

/// `aether-agent status`: are the jobs running, and the last log lines.
pub fn status(_args: &[String]) -> Result<(), String> {
    for job in [daemon_job(), user_agent_job()] {
        println!("{job}: {}", job_state(&job));
    }
    for log in ["daemon", "agent", "loginwindow", "menu"] {
        let path = Path::new(LOGS).join(format!("{log}.log"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        println!("\n--- {}", path.display());
        let lines: Vec<&str> = text.lines().collect();
        for l in &lines[lines.len().saturating_sub(8)..] {
            println!("{l}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_point_at_the_installed_app() {
        let p = plist(&Job {
            label: "x.agent",
            session_type: Some("Aqua"),
            log: "agent.log",
            arg: "agent",
            mach_service: None,
            keep_alive: true,
        });
        assert!(
            p.contains("<string>/Applications/AetherDesk.app/Contents/MacOS/aether-agent</string>")
        );
        assert!(p.contains(
            "<key>AssociatedBundleIdentifiers</key><string>nl.stormlantern.aetherdesk.agent</string>"
        ));
        assert!(p.contains("<key>LimitLoadToSessionType</key><string>Aqua</string>"));
        assert!(p.contains("<string>/Library/Logs/AetherDesk/agent.log</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(!p.contains("MachServices"));
        let d = plist(&Job {
            label: "x.daemon",
            session_type: None,
            log: "daemon.log",
            arg: "daemon",
            mach_service: Some("x.audio"),
            keep_alive: true,
        });
        assert!(!d.contains("LimitLoadToSessionType"));
        assert!(d.contains("<string>daemon</string>"));
        assert!(d.contains("<key>MachServices</key><dict><key>x.audio</key><true/></dict>"));
        let m = plist(&Job {
            label: "x.menu",
            session_type: Some("Aqua"),
            log: "menu.log",
            arg: "menu",
            mach_service: None,
            keep_alive: false,
        });
        assert!(m.contains("<key>KeepAlive</key><false/>"));
    }

    #[test]
    fn only_a_developer_id_signature_is_kept() {
        let release = "Identifier=nl.stormlantern.aetherdesk.agent\n\
                       CodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+2\n\
                       Authority=Developer ID Application: Someone (ABCDE12345)\n\
                       Authority=Developer ID Certification Authority\n\
                       Authority=Apple Root CA\n\
                       TeamIdentifier=ABCDE12345";
        assert!(signed_by_developer_id(release));
        let adhoc = "Identifier=nl.stormlantern.aetherdesk.agent\n\
                     Signature=adhoc\n\
                     TeamIdentifier=not set";
        assert!(!signed_by_developer_id(adhoc));
        let self_signed = "Authority=AetherDesk Dev\nTeamIdentifier=not set";
        assert!(!signed_by_developer_id(self_signed));
    }
}
