//! Daemon ↔ agent messages over a local unix socket. Same framing as the
//! network protocol: a little-endian `u32` length, then a postcard body.
//!
//! The agent connects, says [`FromAgent::Hello`], and waits. The daemon
//! picks the agent of the console session when a client connects and sends
//! [`ToAgent::Start`]; frames flow back until [`ToAgent::Stop`].

use aetherdesk_proto::{InputEvent, VideoFrame};
use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Where the daemon listens for agents.
pub const SOCKET: &str = "/var/run/aetherdesk/agent.sock";

/// Daemon and agent are one binary, installed together; a different build
/// on the other end means a half-finished install.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+ipc1");

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToAgent {
    /// Start capturing the main display and send frames.
    Start {
        fps: u32,
        mbps: u32,
        in_flight: u32,
    },
    /// Stop capturing; the client left.
    Stop,
    RequestKeyframe,
    Input(InputEvent),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FromAgent {
    /// First message after connecting.
    Hello {
        version: String,
    },
    /// Capture is running at this size; frames follow.
    Started {
        width: u32,
        height: u32,
    },
    Frame(VideoFrame),
    /// Capture couldn't start or stopped; the session is over.
    Failed(String),
}

pub async fn write<T: Serialize>(w: &mut (impl AsyncWrite + Unpin), msg: &T) -> anyhow::Result<()> {
    w.write_all(&aetherdesk_proto::encode(msg)?).await?;
    Ok(())
}

/// Reads one message; `Ok(None)` when the other end closed between messages.
pub async fn read<T: DeserializeOwned>(
    r: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Option<T>> {
    let mut prefix = [0u8; 4];
    match r.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut body = vec![0u8; aetherdesk_proto::body_len(prefix)?];
    r.read_exact(&mut body).await.context("reading message")?;
    Ok(Some(aetherdesk_proto::decode_body(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_roundtrip_over_a_socket() {
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let sent = [
            ToAgent::Start {
                fps: 60,
                mbps: 20,
                in_flight: 1,
            },
            ToAgent::Input(InputEvent::Key {
                hid: 0x04,
                down: true,
            }),
            ToAgent::Stop,
        ];
        for m in &sent {
            write(&mut a, m).await.unwrap();
        }
        drop(a);
        for m in &sent {
            assert_eq!(read::<ToAgent>(&mut b).await.unwrap().as_ref(), Some(m));
        }
        assert_eq!(read::<ToAgent>(&mut b).await.unwrap(), None);
    }
}
