//! Wire protocol between the AetherDesk client and host.
//!
//! A connection is one QUIC connection with three streams:
//!
//! - **control** (bidirectional): [`Hello`] → [`Welcome`], then
//!   [`ClientControl`] messages from the client and [`HostControl`] replies.
//! - **video** (host → client): [`VideoFrame`]s.
//! - **input** (client → host): [`InputEvent`]s.
//!
//! Every message is a little-endian `u32` length followed by a postcard body;
//! see [`encode`] and [`decode`], or the [`quic`] helpers (feature `quic`).

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped on every incompatible change. Client and host must match exactly.
pub const PROTOCOL_VERSION: u16 = 3;

/// ALPN protocol name for the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"aetherdesk/1";

/// UDP port the host listens on by default.
pub const DEFAULT_PORT: u16 = 4500;

/// QUIC close code for a host that is stopping (shutdown, restart, uninstall,
/// Ctrl+C), so the client can say so instead of waiting for the idle timeout.
/// Other codes: 0 bye, 1 protocol version mismatch, 2 wrong token, 3 no
/// agent for the console session.
pub const CLOSE_SHUTDOWN: u32 = 4;

/// Upper bound for one message body, so a corrupt length can't make us
/// allocate gigabytes. A 4K keyframe is well under this.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

const LEN_PREFIX: usize = 4;

/// Client → host, first message on the control stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    /// Shared secret the host prints at startup. Keeps other machines on the
    /// network out until M3 brings certificate pinning.
    pub token: String,
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
    /// Clock probe, about once a second. The host answers with
    /// [`HostControl::Pong`]; the client uses the pair to map host clock
    /// times onto its own, which per-stage latency needs.
    Ping { client_us: u64 },
}

/// Host → client on the control stream, after [`Welcome`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostControl {
    /// Answer to [`ClientControl::Ping`]: the client's time echoed back and
    /// the host's clock when it answered.
    Pong { client_us: u64, host_us: u64 },
    /// Which session the picture shows. Sent after [`Welcome`] and whenever
    /// it changes (login, logout); the connection stays up across changes.
    Session(SessionKind),
}

/// The macOS session on screen.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A logged-in user's desktop, or its lock screen: input works.
    User,
    /// The login window after logout or restart. View-only: macOS ignores
    /// synthetic input there (macOS 26.6.2, issue #3).
    LoginWindow,
}

/// When a frame passed each stage on the host, in microseconds on the
/// host's monotonic clock (the same clock as [`HostControl::Pong`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostTiming {
    /// The screen was captured.
    pub captured_us: u64,
    /// The encoder handed the frame back.
    pub encoded_us: u64,
    /// The frame was handed to QUIC.
    pub sent_us: u64,
}

/// One encoded H.264 access unit, Annex B byte stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Capture time in microseconds since the stream started.
    pub pts_us: u64,
    pub keyframe: bool,
    pub timing: HostTiming,
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

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Length of the body that follows a 4-byte prefix, checked against
/// [`MAX_MESSAGE_LEN`]. For readers that pull the prefix and body separately.
pub fn body_len(prefix: [u8; LEN_PREFIX]) -> Result<usize, Error> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(Error::TooLarge(len));
    }
    Ok(len)
}

/// Decodes a message body (without its length prefix).
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    Ok(postcard::from_bytes(body)?)
}

/// Decodes one frame from the start of `buf`.
///
/// Returns `Ok(None)` if `buf` doesn't hold a complete frame yet; otherwise
/// the message and the number of bytes it used, so the caller can drop them.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<Option<(T, usize)>, Error> {
    let Some(prefix) = buf.first_chunk::<LEN_PREFIX>() else {
        return Ok(None);
    };
    let len = body_len(*prefix)?;
    let Some(body) = buf.get(LEN_PREFIX..LEN_PREFIX + len) else {
        return Ok(None);
    };
    Ok(Some((decode_body(body)?, LEN_PREFIX + len)))
}

/// QUIC endpoints and sending/receiving messages on QUIC streams.
#[cfg(feature = "quic")]
pub mod quic {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
    };
    use rustls::{DigitallySignedStruct, SignatureScheme};

    use super::*;

    pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// Server name the client asks for; the host's certificate is made out to it.
    pub const SERVER_NAME: &str = "aetherdesk";

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn transport() -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        t.keep_alive_interval(Some(Duration::from_secs(2)));
        t.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("in range")));
        Arc::new(t)
    }

    /// A listening endpoint with a fresh self-signed certificate.
    pub fn server_endpoint(addr: SocketAddr) -> Result<quinn::Endpoint, BoxError> {
        let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(transport());
        Ok(quinn::Endpoint::server(config, addr)?)
    }

    /// A client endpoint that accepts **any** server certificate.
    ///
    /// Fine for M1 on a LAN or Tailscale; the token still keeps strangers
    /// out, but someone in the middle could watch. M3 replaces this with a
    /// pinned certificate.
    pub fn insecure_client_endpoint(bind: SocketAddr) -> Result<quinn::Endpoint, BoxError> {
        let provider = provider();
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(transport());
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(config);
        Ok(endpoint)
    }

    /// Skips certificate checks but still verifies the handshake signatures.
    #[derive(Debug)]
    struct AcceptAnyCert(Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum StreamError {
        #[error(transparent)]
        Protocol(#[from] Error),
        #[error("stream write: {0}")]
        Write(#[from] quinn::WriteError),
        #[error("stream read: {0}")]
        Read(#[from] quinn::ReadExactError),
    }

    /// Writes one message.
    pub async fn send<T: Serialize>(
        stream: &mut quinn::SendStream,
        msg: &T,
    ) -> Result<(), StreamError> {
        stream.write_all(&encode(msg)?).await?;
        Ok(())
    }

    /// Reads one message; `Ok(None)` when the peer finished the stream cleanly
    /// between messages. Not cancel-safe: don't race it in `select!`.
    pub async fn recv<T: DeserializeOwned>(
        stream: &mut quinn::RecvStream,
    ) -> Result<Option<T>, StreamError> {
        let mut prefix = [0u8; LEN_PREFIX];
        match stream.read_exact(&mut prefix).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let mut body = vec![0u8; body_len(prefix)?];
        stream.read_exact(&mut body).await?;
        Ok(Some(decode_body(&body)?))
    }
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
            token: "0123abcd".into(),
        });
        roundtrip(Welcome {
            version: PROTOCOL_VERSION,
            width: 3456,
            height: 2234,
        });
        roundtrip(ClientControl::RequestKeyframe);
        roundtrip(ClientControl::Ping { client_us: 42 });
        roundtrip(HostControl::Session(SessionKind::LoginWindow));
        roundtrip(HostControl::Pong {
            client_us: 42,
            host_us: 1_000_000,
        });
        roundtrip(VideoFrame {
            pts_us: 16_667,
            keyframe: true,
            timing: HostTiming {
                captured_us: 1,
                encoded_us: 2,
                sent_us: 3,
            },
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
        assert!(matches!(
            body_len(((MAX_MESSAGE_LEN + 1) as u32).to_le_bytes()),
            Err(Error::TooLarge(_))
        ));
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
