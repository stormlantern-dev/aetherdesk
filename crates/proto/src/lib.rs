//! Wire protocol between the AetherDesk client and host.
//!
//! A connection is one QUIC connection with three streams:
//!
//! - **control** (bidirectional): [`Hello`] → [`Welcome`], then [`ClientControl`]
//!   messages from the client.
//! - **video** (host → client): [`VideoFrame`]s.
//! - **input** (client → host): [`InputEvent`]s.
//!
//! Every message is a little-endian `u32` length followed by a postcard body;
//! see [`encode`] and [`decode`].

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped on every incompatible change. Client and host must match exactly.
pub const PROTOCOL_VERSION: u16 = 1;

/// Upper bound for one message body, so a corrupt length can't make us
/// allocate gigabytes. A 4K keyframe is well under this.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

const LEN_PREFIX: usize = 4;

/// Client → host, first message on the control stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
}

/// Host → client, reply to [`Hello`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u16,
    /// Size of the captured display in pixels.
    pub width: u32,
    pub height: u32,
}

/// Client → host on the control stream, after the handshake.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientControl {
    /// Send a keyframe as soon as possible, e.g. after the decoder lost sync.
    RequestKeyframe,
}

/// One encoded H.264 access unit, Annex B byte stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Capture time in microseconds since the stream started.
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Client → host on the input stream.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// USB HID usage ID from the Keyboard/Keypad page (0x07). SDL scancodes
    /// use the same numbers, so the client passes them through unchanged.
    Key {
        hid: u16,
        down: bool,
    },
    /// Absolute pointer position, normalized to `0.0..=1.0` over the display.
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Scroll distance in lines; positive `dy` scrolls up, positive `dx` right.
    Scroll {
        dx: f32,
        dy: f32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("message of {0} bytes exceeds the {MAX_MESSAGE_LEN} byte limit")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
}

/// Encodes `msg` as one length-prefixed frame.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, Error> {
    let mut out = postcard::to_extend(msg, vec![0; LEN_PREFIX])?;
    let len = out.len() - LEN_PREFIX;
    if len > MAX_MESSAGE_LEN {
        return Err(Error::TooLarge(len));
    }
    out[..LEN_PREFIX].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(out)
}

/// Decodes one frame from the start of `buf`.
///
/// Returns `Ok(None)` if `buf` doesn't hold a complete frame yet; otherwise
/// the message and the number of bytes it used, so the caller can drop them.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<Option<(T, usize)>, Error> {
    let Some(prefix) = buf.first_chunk::<LEN_PREFIX>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(*prefix) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(Error::TooLarge(len));
    }
    let Some(body) = buf.get(LEN_PREFIX..LEN_PREFIX + len) else {
        return Ok(None);
    };
    Ok(Some((postcard::from_bytes(body)?, LEN_PREFIX + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(msg: T) {
        let bytes = encode(&msg).unwrap();
        let (back, used) = decode::<T>(&bytes).unwrap().unwrap();
        assert_eq!(back, msg);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn messages_roundtrip() {
        roundtrip(Hello {
            version: PROTOCOL_VERSION,
        });
        roundtrip(Welcome {
            version: PROTOCOL_VERSION,
            width: 3456,
            height: 2234,
        });
        roundtrip(ClientControl::RequestKeyframe);
        roundtrip(VideoFrame {
            pts_us: 16_667,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67],
        });
        roundtrip(InputEvent::Key {
            hid: 0xE3,
            down: true,
        });
        roundtrip(InputEvent::MouseMove { x: 0.25, y: 1.0 });
        roundtrip(InputEvent::MouseButton {
            button: MouseButton::Right,
            down: false,
        });
        roundtrip(InputEvent::Scroll { dx: 0.0, dy: -3.0 });
    }

    #[test]
    fn partial_input_needs_more_bytes() {
        let bytes = encode(&InputEvent::Key { hid: 4, down: true }).unwrap();
        for cut in 0..bytes.len() {
            assert!(
                decode::<InputEvent>(&bytes[..cut]).unwrap().is_none(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn decodes_back_to_back_frames() {
        let a = InputEvent::Key { hid: 4, down: true };
        let b = InputEvent::Key {
            hid: 4,
            down: false,
        };
        let mut buf = encode(&a).unwrap();
        buf.extend(encode(&b).unwrap());

        let (first, used) = decode::<InputEvent>(&buf).unwrap().unwrap();
        let (second, rest) = decode::<InputEvent>(&buf[used..]).unwrap().unwrap();
        assert_eq!((first, second), (a, b));
        assert_eq!(used + rest, buf.len());
    }

    #[test]
    fn rejects_oversized_length() {
        let mut buf = ((MAX_MESSAGE_LEN + 1) as u32).to_le_bytes().to_vec();
        buf.push(0);
        assert!(matches!(decode::<Hello>(&buf), Err(Error::TooLarge(_))));
    }

    #[test]
    fn rejects_garbage_body() {
        let buf = [1, 0, 0, 0, 0xFF];
        assert!(matches!(
            decode::<ClientControl>(&buf),
            Err(Error::Malformed(_))
        ));
    }
}
