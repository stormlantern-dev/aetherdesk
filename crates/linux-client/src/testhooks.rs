//! Test-only remote control of the client window. Only in builds with the
//! cargo feature `test-hooks` (CI and local testing); release builds have no
//! trace of it, so nothing on the machine can drive a session through it.
//!
//! With `AETHERDESK_TEST_SOCKET=/path` set, the client listens on that unix
//! socket (mode 0600). One command per line, one answer line each:
//!
//! - `key NAME`: press and release a key. NAME is an SDL scancode name;
//!   `_` stands for a space: `Right_Ctrl`, `Escape`, `Return`, `A`.
//! - `down NAME`, `up NAME`: just one half.
//! - `click X Y`: left click at window coordinates (logical pixels).
//! - `snapshot PATH`: write what the window shows now, overlay included, as
//!   PNG. Answers `ok WxH`.
//! - `state`: connection, grab and overlay as one line of JSON.
//! - `copy TEXT`: put TEXT (the rest of the line, `\\n` for a line break)
//!   on the clipboard as if another app copied it. `copybig N` does so with N bytes of `x`.
//! - `paste`: what's on the clipboard, as `ok "<JSON string>"`.
//! - `resize W H`: make the window W×H (logical pixels), as a user would.
//!
//! Events go through the same handling as real ones.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use sdl3::keyboard::{Mod, Scancode};
use sdl3::mouse::MouseButton;
use sdl3::pixels::PixelFormat;
use sdl3::render::WindowCanvas;

pub enum Command {
    Key { scancode: Scancode, down: bool },
    Click { x: f32, y: f32, down: bool },
    Snapshot(PathBuf),
    State,
    Copy(String),
    Paste,
    Resize(u32, u32),
}

/// A command and where its answer goes.
pub type Request = (Command, Sender<String>);

pub struct Hooks {
    requests: Receiver<Request>,
}

impl Hooks {
    /// Starts listening if `AETHERDESK_TEST_SOCKET` is set.
    pub fn start() -> Option<Self> {
        let path = std::env::var_os("AETHERDESK_TEST_SOCKET")?;
        let path = PathBuf::from(path);
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("test hooks: binding {}: {e}", path.display());
                return None;
            }
        };
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        println!("test hooks listening on {}", path.display());
        let (tx, requests) = channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                std::thread::spawn(move || serve(stream, &tx));
            }
        });
        Some(Self { requests })
    }

    /// Commands that arrived since the last call.
    pub fn pending(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

fn serve(stream: std::os::unix::net::UnixStream, tx: &Sender<Request>) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let answer = match parse(&line) {
            Ok(commands) => {
                let mut last = String::from("ok");
                for c in commands {
                    let (reply, answer) = channel();
                    if tx.send((c, reply)).is_err() {
                        return;
                    }
                    last = answer
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| "error: the window didn't answer".into());
                }
                last
            }
            Err(e) => format!("error: {e}"),
        };
        if writeln!(out, "{answer}").is_err() {
            return;
        }
    }
}

fn parse(line: &str) -> Result<Vec<Command>, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let scancode = |name: &str| {
        Scancode::from_name(&name.replace('_', " ")).ok_or_else(|| format!("unknown key {name:?}"))
    };
    match words.as_slice() {
        ["key", name] => {
            let sc = scancode(name)?;
            Ok(vec![
                Command::Key {
                    scancode: sc,
                    down: true,
                },
                Command::Key {
                    scancode: sc,
                    down: false,
                },
            ])
        }
        ["down", name] => Ok(vec![Command::Key {
            scancode: scancode(name)?,
            down: true,
        }]),
        ["up", name] => Ok(vec![Command::Key {
            scancode: scancode(name)?,
            down: false,
        }]),
        ["click", x, y] => {
            let x = x.parse().map_err(|_| "click X Y wants numbers")?;
            let y = y.parse().map_err(|_| "click X Y wants numbers")?;
            Ok(vec![
                Command::Click { x, y, down: true },
                Command::Click { x, y, down: false },
            ])
        }
        ["snapshot", path] => Ok(vec![Command::Snapshot(PathBuf::from(path))]),
        ["state"] => Ok(vec![Command::State]),
        ["paste"] => Ok(vec![Command::Paste]),
        ["resize", w, h] => {
            let w = w.parse().map_err(|_| "resize W H wants numbers")?;
            let h = h.parse().map_err(|_| "resize W H wants numbers")?;
            Ok(vec![Command::Resize(w, h)])
        }
        ["copybig", n] => {
            let n = n.parse().map_err(|_| "copybig N wants a number")?;
            Ok(vec![Command::Copy("x".repeat(n))])
        }
        ["copy", ..] => {
            let text = line.trim_start().strip_prefix("copy").unwrap_or("");
            let text = text.strip_prefix(' ').unwrap_or(text).replace("\\n", "\n");
            Ok(vec![Command::Copy(text)])
        }
        _ => Err(format!("unknown command {line:?}")),
    }
}

/// The SDL event a command stands for, if it's an input command.
pub fn event(command: &Command, window_id: u32) -> Option<sdl3::event::Event> {
    use sdl3::event::Event;
    match *command {
        Command::Key { scancode, down } => Some(if down {
            Event::KeyDown {
                timestamp: 0,
                window_id,
                keycode: None,
                scancode: Some(scancode),
                keymod: Mod::NOMOD,
                repeat: false,
                which: 0,
                raw: 0,
            }
        } else {
            Event::KeyUp {
                timestamp: 0,
                window_id,
                keycode: None,
                scancode: Some(scancode),
                keymod: Mod::NOMOD,
                repeat: false,
                which: 0,
                raw: 0,
            }
        }),
        Command::Click { x, y, down } => Some(if down {
            Event::MouseButtonDown {
                timestamp: 0,
                window_id,
                which: 0,
                mouse_btn: MouseButton::Left,
                clicks: 1,
                x,
                y,
            }
        } else {
            Event::MouseButtonUp {
                timestamp: 0,
                window_id,
                which: 0,
                mouse_btn: MouseButton::Left,
                clicks: 1,
                x,
                y,
            }
        }),
        _ => None,
    }
}

/// Writes the current render target as PNG. Call after drawing, before
/// `present` (after it, the back buffer is undefined), with the logical
/// presentation switched off so the whole window is read.
pub fn snapshot(canvas: &WindowCanvas, path: &Path) -> Result<(u32, u32), String> {
    let surface = canvas.read_pixels(None).map_err(|e| e.to_string())?;
    let surface = surface
        .convert_format(PixelFormat::RGBA32)
        .map_err(|e| e.to_string())?;
    let (w, h, pitch) = (surface.width(), surface.height(), surface.pitch() as usize);
    let rgba: Vec<u8> = surface.with_lock(|bytes| {
        bytes
            .chunks(pitch)
            .take(h as usize)
            .flat_map(|row| &row[..w as usize * 4])
            .copied()
            .collect()
    });
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok((w, h))
}

/// A string as a JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse("key Right_Ctrl").unwrap().len(), 2);
        assert_eq!(parse("down Escape").unwrap().len(), 1);
        assert_eq!(parse("click 10 20").unwrap().len(), 2);
        assert!(parse("key Nope").is_err());
        assert!(parse("dance").is_err());
        assert!(matches!(parse("state").unwrap()[0], Command::State));
        assert!(matches!(&parse("copy a b\\nc").unwrap()[0], Command::Copy(t) if t == "a b\nc"));
        assert!(matches!(&parse("copybig 3").unwrap()[0], Command::Copy(t) if t == "xxx"));
        assert!(matches!(parse("paste").unwrap()[0], Command::Paste));
        assert!(matches!(
            parse("resize 800 600").unwrap()[0],
            Command::Resize(800, 600)
        ));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
