//! Installing the host as launchd jobs: a root daemon (the network side)
//! and an agent in every GUI session (login window and user), all running
//! one copy of this binary from `/Library/Application Support/AetherDesk`.
//!
//! `aether-agent install` works from wherever the binary is (a Homebrew
//! cask, a cargo build): it signs a copy, then asks for admin rights for the
//! part that needs them. `scripts/mac.zsh install` builds and calls this.

use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "nl.stormlantern.aetherdesk";
const DEST: &str = "/Library/Application Support/AetherDesk";
const LOGS: &str = "/Library/Logs/AetherDesk";

fn bin() -> PathBuf {
    Path::new(DEST).join("aether-agent")
}

fn daemon_plist() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.daemon.plist"))
}

fn agent_plists() -> [PathBuf; 2] {
    [
        PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.agent.plist")),
        PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.loginwindow.plist")),
    ]
}

pub const USAGE: &str = "\
aether-agent install     install (or update) the daemon and agents; asks for
                         your password. Signs with the code-signing identity
                         $AETHERDESK_IDENTITY (default \"AetherDesk Dev\") when
                         you have one, so permissions survive updates;
                         otherwise ad hoc, and macOS asks for them again after
                         each update.
aether-agent uninstall   remove them again (pairings included)
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

fn plist(label: &str, session_type: Option<&str>, log: &str, arg: &str) -> String {
    let limit = session_type
        .map(|t| format!("  <key>LimitLoadToSessionType</key><string>{t}</string>\n"))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
    <string>{bin}</string>
    <string>{arg}</string>
  </array>
{limit}  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{LOGS}/{log}</string>
  <key>StandardErrorPath</key><string>{LOGS}/{log}</string>
</dict></plist>
"#,
        bin = bin().display()
    )
}

/// `aether-agent install`, as the user: signs a copy of this binary and
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
    let me = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let stage = std::env::temp_dir().join(format!("aetherdesk-install-{}", std::process::id()));
    std::fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let staged = stage.join("aether-agent");
    std::fs::copy(&me, &staged).map_err(|e| format!("copying {}: {e}", me.display()))?;
    let staged_s = staged.to_string_lossy().to_string();
    // A cask's download carries the quarantine flag; the copy shouldn't.
    let _ = run("xattr", &["-d", "com.apple.quarantine", &staged_s]);

    // One identifier for every build, so permissions stick to it.
    let identifier = format!("{LABEL}.agent");
    let identity = std::env::var("AETHERDESK_IDENTITY").unwrap_or_else(|_| "AetherDesk Dev".into());
    let have_identity = Command::new("security")
        .args(["find-identity", "-p", "codesigning"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{identity}\"")));
    let sign_as = if have_identity {
        identity.as_str()
    } else {
        "-"
    };
    run(
        "codesign",
        &[
            "--force",
            "--sign",
            sign_as,
            "--identifier",
            &identifier,
            &staged_s,
        ],
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

    println!("Installing needs admin rights; macOS asks for your password.");
    let status = Command::new("sudo")
        .arg(&staged)
        .args(["_install-root", &staged_s])
        .status()
        .map_err(|e| format!("sudo: {e}"))?;
    let _ = std::fs::remove_dir_all(&stage);
    if !status.success() {
        return Err("the admin part didn't finish".into());
    }
    // The user's agent: launchd loads it by itself at every login; this
    // starts it in the current session.
    let job = format!("gui/{}/{LABEL}.agent", uid());
    unload(&job);
    let [agent, _] = agent_plists();
    run(
        "launchctl",
        &[
            "bootstrap",
            &format!("gui/{}", uid()),
            &agent.to_string_lossy(),
        ],
    )?;
    println!(
        "\nInstalled. Once, grant {}\nboth Screen Recording and Accessibility in System \
         Settings → Privacy & Security (+ button, then Cmd+Shift+G and paste the path), then \
         run: aether-agent status\nTo let a client in: sudo aether-agent pair",
        bin().display()
    );
    Ok(())
}

/// The root part of `install`: puts the signed copy in place, writes the
/// launchd jobs and starts the daemon.
pub fn install_root(args: &[String]) -> Result<(), String> {
    let [staged] = args else {
        return Err("_install-root needs the staged binary".into());
    };
    if uid() != 0 {
        return Err("_install-root runs as root, from install".into());
    }
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    std::fs::create_dir_all(DEST).map_err(|e| io(DEST, e))?;
    std::fs::create_dir_all(LOGS).map_err(|e| io(LOGS, e))?;
    // The user's agent writes its log here too.
    run("chmod", &["1777", LOGS])?;
    unload(&format!("system/{LABEL}.daemon"));
    // A new inode on every install: overwriting a signed binary in place
    // can get it killed.
    let _ = std::fs::remove_file(bin());
    std::fs::copy(staged, bin()).map_err(|e| io("installing the binary", e))?;
    let bin_s = bin().to_string_lossy().to_string();
    run("chown", &["root:wheel", &bin_s])?;
    run("chmod", &["755", &bin_s])?;
    // M2's shared token, replaced by pairing.
    let _ = std::fs::remove_file(Path::new(DEST).join("token"));
    let [agent, loginwindow] = agent_plists();
    let jobs = [
        (
            daemon_plist(),
            plist(&format!("{LABEL}.daemon"), None, "daemon.log", "daemon"),
        ),
        (
            agent,
            plist(
                &format!("{LABEL}.agent"),
                Some("Aqua"),
                "agent.log",
                "agent",
            ),
        ),
        (
            loginwindow,
            plist(
                &format!("{LABEL}.loginwindow"),
                Some("LoginWindow"),
                "loginwindow.log",
                "agent",
            ),
        ),
    ];
    for (path, text) in &jobs {
        std::fs::write(path, text).map_err(|e| io(&path.to_string_lossy(), e))?;
        let p = path.to_string_lossy();
        run("chown", &["root:wheel", &p])?;
        run("chmod", &["644", &p])?;
    }
    run(
        "launchctl",
        &["bootstrap", "system", &daemon_plist().to_string_lossy()],
    )
}

/// `aether-agent uninstall`, as the user.
pub fn uninstall(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.into());
    }
    unload(&format!("gui/{}/{LABEL}.agent", uid()));
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
    println!("Removed. The aether-agent entries in Privacy & Security stay until you remove them.");
    Ok(())
}

/// The root part of `uninstall`.
pub fn uninstall_root(_args: &[String]) -> Result<(), String> {
    if uid() != 0 {
        return Err("_uninstall-root runs as root, from uninstall".into());
    }
    unload(&format!("system/{LABEL}.daemon"));
    let [agent, loginwindow] = agent_plists();
    for p in [daemon_plist(), agent, loginwindow] {
        let _ = std::fs::remove_file(p);
    }
    for dir in [DEST, LOGS, "/var/run/aetherdesk"] {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

/// `aether-agent status`: are the jobs running, and the last log lines.
pub fn status(_args: &[String]) -> Result<(), String> {
    for job in [
        format!("system/{LABEL}.daemon"),
        format!("gui/{}/{LABEL}.agent", uid()),
    ] {
        match Command::new("launchctl").args(["print", &job]).output() {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let fields: Vec<&str> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with("state =") || l.starts_with("pid ="))
                    .collect();
                println!("{job}: {}", fields.join(", "));
            }
            _ => println!("{job}: not loaded"),
        }
    }
    for log in ["daemon", "agent", "loginwindow"] {
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
    fn plists_point_at_the_installed_binary() {
        let p = plist("x.agent", Some("Aqua"), "agent.log", "agent");
        assert!(
            p.contains("<string>/Library/Application Support/AetherDesk/aether-agent</string>")
        );
        assert!(p.contains("<key>LimitLoadToSessionType</key><string>Aqua</string>"));
        assert!(p.contains("<string>/Library/Logs/AetherDesk/agent.log</string>"));
        let d = plist("x.daemon", None, "daemon.log", "daemon");
        assert!(!d.contains("LimitLoadToSessionType"));
        assert!(d.contains("<string>daemon</string>"));
    }
}
