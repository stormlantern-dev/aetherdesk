//! One video picture as a PNG, the same way on both sides: the host saves
//! what goes into the encoder, the client what comes out of the decoder
//! (`AUROVAL_DUMP_FRAME`, #113). Comparing the two shows whether
//! softness comes from the scaling or from the encoding.
//!
//! 4:2:0 pictures are converted with BT.709, limited range (what the host
//! captures and encodes), chroma repeated per 2×2 block, so the PNG shows
//! the subsampling as it is; BGRA ones are copied.

use std::path::{Path, PathBuf};

/// A picture's pixels: planes and their bytes per row.
pub enum Pixels<'a> {
    /// Y, then U and V interleaved (half size each way).
    Nv12 {
        y: (&'a [u8], usize),
        uv: (&'a [u8], usize),
    },
    /// Y, U, V (U and V half size each way).
    I420 {
        y: (&'a [u8], usize),
        u: (&'a [u8], usize),
        v: (&'a [u8], usize),
    },
    /// 4 bytes per pixel: B, G, R, A.
    Bgra((&'a [u8], usize)),
}

/// `PREFIX-WHAT-WxH.png`.
pub fn path(prefix: &str, what: &str, size: (u32, u32)) -> PathBuf {
    PathBuf::from(format!("{prefix}-{what}-{}x{}.png", size.0, size.1))
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = (y as f32 - 16.0) * 1.164_384;
    let (u, v) = (u as f32 - 128.0, v as f32 - 128.0);
    let px = |f: f32| f.round().clamp(0.0, 255.0) as u8;
    [
        px(y + 1.792_741 * v),
        px(y - 0.213_249 * u - 0.532_909 * v),
        px(y + 2.112_402 * u),
    ]
}

/// `width`×`height` pixels as packed RGB.
pub fn to_rgb(pixels: &Pixels, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        for col in 0..width {
            let rgb = match pixels {
                Pixels::Nv12 { y, uv } => {
                    let c = (row / 2) * uv.1 + (col / 2) * 2;
                    yuv_to_rgb(y.0[row * y.1 + col], uv.0[c], uv.0[c + 1])
                }
                Pixels::I420 { y, u, v } => yuv_to_rgb(
                    y.0[row * y.1 + col],
                    u.0[(row / 2) * u.1 + col / 2],
                    v.0[(row / 2) * v.1 + col / 2],
                ),
                Pixels::Bgra((data, stride)) => {
                    let p = row * stride + col * 4;
                    [data[p + 2], data[p + 1], data[p]]
                }
            };
            out.extend_from_slice(&rgb);
        }
    }
    out
}

/// Writes packed RGB as an 8-bit PNG.
pub fn write_png(path: &Path, width: u32, height: u32, rgb: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_range_maps_to_full_range() {
        assert_eq!(yuv_to_rgb(16, 128, 128), [0, 0, 0]);
        assert_eq!(yuv_to_rgb(235, 128, 128), [255, 255, 255]);
        // BT.709 red, limited range.
        let [r, g, b] = yuv_to_rgb(63, 102, 240);
        assert!(r > 250 && g < 5 && b < 5, "{r} {g} {b}");
    }

    #[test]
    fn layouts_agree() {
        // 2×2 grey picture with a stride wider than the row.
        let y = [100u8, 100, 0, 0, 100, 100, 0, 0];
        let uv = [128u8, 128, 0, 0];
        let u = [128u8, 0];
        let v = [128u8, 0];
        let nv12 = to_rgb(
            &Pixels::Nv12 {
                y: (&y, 4),
                uv: (&uv, 4),
            },
            2,
            2,
        );
        let i420 = to_rgb(
            &Pixels::I420 {
                y: (&y, 4),
                u: (&u, 2),
                v: (&v, 2),
            },
            2,
            2,
        );
        assert_eq!(nv12, i420);
        assert_eq!(nv12.len(), 12);
        let bgra = [1u8, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(to_rgb(&Pixels::Bgra((&bgra, 8)), 2, 1), [3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn writes_a_png() {
        let dir = std::env::temp_dir().join(format!("auroval-still-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = path(dir.join("t").to_str().unwrap(), "decoded", (2, 1));
        assert!(p.to_str().unwrap().ends_with("t-decoded-2x1.png"));
        write_png(&p, 2, 1, &[0, 0, 0, 255, 255, 255]).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
