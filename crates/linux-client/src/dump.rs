//! `AUROVAL_DUMP_FRAME=PREFIX`: saves one decoded picture per picture
//! size as `PREFIX-decoded-WxH.png`, as it comes out of the decoder (before
//! any sharpening and before the window scales it), for comparing with what
//! the host captured (#113). It's the newest picture once the size has
//! lasted `AUROVAL_DUMP_AFTER` seconds (default 2), so the encoder had
//! time to refine the keyframe after a resize; also when the screen stood
//! still and no picture came after that.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use auroval_proto::still::{self, Pixels};

use crate::decoder::{Layout, Picture};

const DEFAULT_AFTER: Duration = Duration::from_secs(2);

pub struct Dump {
    /// The current size, when it started, whether its picture is sent off.
    size: Option<((u32, u32), Instant, bool)>,
    after: Duration,
    to_writer: Sender<(Instant, Picture)>,
}

impl Dump {
    pub fn from_env() -> Option<Self> {
        let prefix = std::env::var("AUROVAL_DUMP_FRAME")
            .ok()
            .filter(|p| !p.is_empty())?;
        let after = std::env::var("AUROVAL_DUMP_AFTER")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map_or(DEFAULT_AFTER, Duration::from_secs_f64);
        say!(
            "saving a decoded picture per size, {:.1} s after it starts (AUROVAL_DUMP_FRAME)",
            after.as_secs_f64()
        );
        Self::new(prefix, after)
    }

    fn new(prefix: String, after: Duration) -> Option<Self> {
        let (to_writer, pictures) = channel();
        std::thread::Builder::new()
            .name("dump".into())
            .spawn(move || write_when_due(&prefix, &pictures))
            .ok()?;
        Some(Self {
            size: None,
            after,
            to_writer,
        })
    }

    /// Hands a copy of `p` to the writer while its size's picture is still
    /// to be saved.
    pub fn offer(&mut self, p: &Picture) {
        let now = Instant::now();
        let size = (p.width, p.height);
        if self.size.is_none_or(|(s, _, _)| s != size) {
            self.size = Some((size, now, false));
        }
        let Some((_, since, sent)) = &mut self.size else {
            return;
        };
        if *sent {
            return;
        }
        let due = *since + self.after;
        // The first picture at or after the deadline is the last one needed.
        if now >= due {
            *sent = true;
        }
        let _ = self.to_writer.send((due, copy(p)));
    }
}

fn copy(p: &Picture) -> Picture {
    Picture {
        width: p.width,
        height: p.height,
        layout: p.layout,
        planes: p.planes.clone(),
        strides: p.strides,
        timing: p.timing,
    }
}

/// Keeps the newest picture and writes it when it's due; one per size.
fn write_when_due(prefix: &str, pictures: &Receiver<(Instant, Picture)>) {
    let mut kept: Option<(Instant, Picture)> = None;
    let mut written: Vec<(u32, u32)> = Vec::new();
    loop {
        let next = match &kept {
            Some((due, _)) => pictures.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => pictures.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok((due, p)) => {
                if !written.contains(&(p.width, p.height)) {
                    kept = Some((due, p));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some((due, _)) = &kept
            && Instant::now() >= *due
        {
            let (_, p) = kept.take().expect("just checked");
            written.push((p.width, p.height));
            let path = still::path(prefix, "decoded", (p.width, p.height));
            let rgb = still::to_rgb(&pixels(&p), p.width as usize, p.height as usize);
            match still::write_png(&path, p.width, p.height, &rgb) {
                Ok(()) => say!("saved {}", path.display()),
                Err(e) => warn!("dump: {e}"),
            }
        }
    }
}

fn pixels(p: &Picture) -> Pixels<'_> {
    match p.layout {
        Layout::I420 => Pixels::I420 {
            y: (&p.planes[0], p.strides[0]),
            u: (&p.planes[1], p.strides[1]),
            v: (&p.planes[2], p.strides[2]),
        },
        Layout::Nv12 => Pixels::Nv12 {
            y: (&p.planes[0], p.strides[0]),
            uv: (&p.planes[1], p.strides[1]),
        },
        Layout::Bgra => Pixels::Bgra((&p.planes[0], p.strides[0])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::Timing;

    fn picture(w: u32, h: u32, luma: u8) -> Picture {
        let (w2, h2) = (w as usize, h as usize);
        Picture {
            width: w,
            height: h,
            layout: Layout::I420,
            planes: [
                vec![luma; w2 * h2],
                vec![128; w2 * h2 / 4],
                vec![128; w2 * h2 / 4],
            ],
            strides: [w2, w2 / 2, w2 / 2],
            timing: Timing::default(),
        }
    }

    /// The newest picture of a size is saved once it's due, also when no
    /// picture comes after the deadline (a still screen); one per size.
    #[test]
    fn saves_the_newest_picture_per_size() {
        let dir = std::env::temp_dir().join(format!("auroval-dump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prefix = dir.join("t").to_str().unwrap().to_string();
        let mut dump = Dump::new(prefix.clone(), Duration::from_millis(200)).unwrap();
        dump.offer(&picture(4, 2, 16));
        dump.offer(&picture(4, 2, 235));
        let file = still::path(&prefix, "decoded", (4, 2));
        std::thread::sleep(Duration::from_millis(600));
        let decoded = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(&file).expect("saved after the deadline"),
        ));
        let mut reader = decoded.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        // The newer, white one.
        assert_eq!(&buf[..3], &[255, 255, 255]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
