//! The session log: what the client prints while connected (connection,
//! video format, latency, sizes of clipboard copies) is also kept in
//! `$XDG_STATE_HOME/aetherdesk/client.log` (usually `~/.local/state`), so it
//! can be read afterwards instead of copied out of a terminal. Started fresh
//! for every session, readable by the user only, never sent anywhere. It
//! holds nothing the client doesn't print: no clipboard text, no keys.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

static LOG: Mutex<Option<(File, Instant)>> = Mutex::new(None);

/// Where the log goes.
pub fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("aetherdesk/client.log"))
}

/// Starts a new log for this session; an old one is overwritten.
pub fn start() {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path);
    if let Ok(f) = file {
        *LOG.lock().unwrap() = Some((f, Instant::now()));
    }
}

/// Keeps `line`, with the seconds since the session started.
pub fn keep(line: &str) {
    if let Some((f, start)) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{:8.1}s {line}", start.elapsed().as_secs_f64());
    }
}
