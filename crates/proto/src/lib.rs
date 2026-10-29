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
pub const PROTOCOL_VERSION: u16 = 8;

/// ALPN protocol name for the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"aetherdesk/1";

/// UDP port the host listens on by default.
pub const DEFAULT_PORT: u16 = 4500;

// QUIC close codes the host uses, so the client can say what happened.
// 0 is a plain goodbye.

/// The client speaks a different [`PROTOCOL_VERSION`].
pub const CLOSE_VERSION: u32 = 1;
/// The client's certificate isn't paired with this host.
pub const CLOSE_NOT_PAIRED: u32 = 2;
/// No agent for the session on screen (the agent isn't installed or running).
pub const CLOSE_NO_AGENT: u32 = 3;
/// The host is stopping (shutdown, restart, uninstall, Ctrl+C).
pub const CLOSE_SHUTDOWN: u32 = 4;
/// The client asked to pair, but pairing isn't open on the host.
pub const CLOSE_PAIRING_CLOSED: u32 = 5;
/// Pairing failed: the code didn't match.
pub const CLOSE_WRONG_CODE: u32 = 6;
/// The client decodes none of the video formats the host can send.
pub const CLOSE_CODEC: u32 = 7;

/// Upper bound for one message body, so a corrupt length can't make us
/// allocate gigabytes. A 4K keyframe is well under this.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// Largest clipboard text either side sends, in bytes. Bigger copies are
/// skipped (with a log line), not cut off: half a paste is worse than none.
pub const MAX_CLIPBOARD: usize = 1024 * 1024;

const LEN_PREFIX: usize = 4;

/// Client → host, first message on the control stream. Who the client is
/// comes from its TLS certificate, not from this message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub intent: Intent,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Start a session; the host answers with [`Welcome`] if this client is
    /// paired. With `clipboard`, both sides pass on text copied during the
    /// session ([`ClientControl::Clipboard`], [`HostControl::Clipboard`]);
    /// without it the host sends none. `codecs`: the video formats the
    /// client can decode, best first; the host takes the first it can send
    /// ([`Codec::pick`]) and names it in [`Welcome::codec`].
    Connect { clipboard: bool, codecs: Vec<Codec> },
    /// Pair with the host while it shows a code; [`PairMsg`]s follow.
    /// `name` is how the host will list this client.
    Pair { name: String },
}

/// Pairing, on the control stream after [`Intent::Pair`]:
///
/// 1. client → host [`PairMsg::Spake`], host → client [`PairMsg::Spake`]:
///    both sides run SPAKE2 with the code and get the same key only if they
///    used the same code. The code itself never crosses the wire.
/// 2. client → host [`PairMsg::Confirm`]: proof of the key, bound to both
///    TLS certificates, so someone in the middle can't relay it.
/// 3. host → client [`PairMsg::Paired`]: the host's own proof and name.
///
/// On a wrong code the host closes with [`CLOSE_WRONG_CODE`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PairMsg {
    Spake(Vec<u8>),
    Confirm(Vec<u8>),
    Paired { name: String, confirm: Vec<u8> },
}

/// Host → client, reply to [`Hello`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u16,
    /// Size of the captured display in pixels.
    pub width: u32,
    pub height: u32,
    /// The format every [`VideoFrame`] of this session is in.
    pub codec: Codec,
}

/// A video format: codec and chroma subsampling. Annex B byte stream.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 High, 4:2:0. Every client and host can do this one.
    H264,
    /// HEVC Main, 4:2:0.
    Hevc,
    /// HEVC Main 4:4:4: full colour resolution, so small text stays sharp.
    Hevc444,
}

impl Codec {
    /// The first of `offered` (the client's, best first) that the host can
    /// send, if any.
    pub fn pick(offered: &[Codec], ours: &[Codec]) -> Option<Codec> {
        offered.iter().copied().find(|c| ours.contains(c))
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Codec::H264 => "H.264 4:2:0",
            Codec::Hevc => "HEVC 4:2:0",
            Codec::Hevc444 => "HEVC 4:4:4",
        })
    }
}

/// Client → host on the control stream, after the handshake.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ClientControl {
    /// Send a keyframe as soon as possible, e.g. after the decoder lost sync.
    RequestKeyframe,
    /// Clock probe, about once a second. The host answers with
    /// [`HostControl::Pong`]; the client uses the pair to map host clock
    /// times onto its own, which per-stage latency needs.
    Ping { client_us: u64 },
    /// Text copied on the client, for the Mac's clipboard. At most
    /// [`MAX_CLIPBOARD`] bytes; only when the session asked for clipboard.
    Clipboard(String),
}

/// Host → client on the control stream, after [`Welcome`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum HostControl {
    /// Answer to [`ClientControl::Ping`]: the client's time echoed back and
    /// the host's clock when it answered.
    Pong { client_us: u64, host_us: u64 },
    /// Which session the picture shows. Sent after [`Welcome`] and whenever
    /// it changes (login, logout); the connection stays up across changes.
    Session(SessionKind),
    /// Text copied on the Mac, for the client's clipboard. At most
    /// [`MAX_CLIPBOARD`] bytes; only when the client asked for clipboard.
    Clipboard(String),
    /// The pointer's shape changed to one not sent before. From the first
    /// one on, the video no longer contains the pointer: the client draws
    /// it, so it follows the hand without the round trip. A host that can't
    /// read the shape never sends this, and the pointer stays in the video.
    CursorImage(CursorImage),
    /// The pointer's shape changed to one sent before, by its id.
    CursorShow(u64),
    /// The Mac moved the pointer itself (an app warped it, or the session
    /// just started): where it is now, `0.0..=1.0` over the picture.
    CursorMoved { x: f32, y: f32 },
    /// The pointer is in the video again (e.g. the login window took over,
    /// where the shape can't be read): stop drawing it locally.
    CursorInVideo,
}

/// A pointer shape, in the captured display's pixels (2× on a Retina Mac).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    /// Stable for the same image, so it's sent once and then named.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// The pixel that points.
    pub hot_x: u32,
    pub hot_y: u32,
    /// Straight (not premultiplied) RGBA, row by row, no padding.
    pub rgba: Vec<u8>,
}

/// Keeps clipboard sync from echoing back and forth. Each side holds one:
/// text that came from the other side is [`noted`](Self::noted) before it's
/// put on the local clipboard, and a local change is only sent when
/// [`is_new`](Self::is_new) says it differs from what's already in sync.
#[derive(Debug, Default)]
pub struct ClipboardEcho {
    last: Option<u64>,
}

impl ClipboardEcho {
    fn hash(text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        h.finish()
    }

    /// Text from the other side, about to be put on our clipboard.
    pub fn noted(&mut self, text: &str) {
        self.last = Some(Self::hash(text));
    }

    /// Our clipboard changed to `text`: true when it's a new copy to send,
    /// false when it's what we just put there (or sent) ourselves.
    pub fn is_new(&mut self, text: &str) -> bool {
        let h = Self::hash(text);
        if self.last == Some(h) {
            return false;
        }
        self.last = Some(h);
        true
    }
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
    /// The screen was captured (the frame's presentation time).
    pub captured_us: u64,
    /// ScreenCaptureKit handed the frame to us.
    pub delivered_us: u64,
    /// It went into the encoder (later than delivered when the encoder was
    /// still busy with the one before).
    pub submitted_us: u64,
    /// The encoder handed the frame back.
    pub encoded_us: u64,
    /// The frame was handed to QUIC.
    pub sent_us: u64,
}

/// One encoded access unit in the session's [`Codec`], Annex B byte stream.
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

#[cfg(feature = "quic")]
pub mod trust;

#[cfg(feature = "quic")]
pub mod discovery;

/// QUIC endpoints and sending/receiving messages on QUIC streams.
#[cfg(feature = "quic")]
pub mod quic {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};

    use super::*;
    use crate::trust::{Fingerprint, Identity};

    pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// Server name the client asks for; the host's certificate is made out to it.
    pub const SERVER_NAME: &str = "aetherdesk";

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn transport() -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        // A dead link shows within ~5 s: the client then reconnects, and
        // the host stops capturing. Keep-alives every second keep a quiet
        // link (still screen, no input) from looking dead.
        t.keep_alive_interval(Some(Duration::from_secs(1)));
        t.max_idle_timeout(Some(Duration::from_secs(5).try_into().expect("in range")));
        Arc::new(t)
    }

    /// A listening endpoint with this host's certificate. Every client must
    /// present a certificate; whether it is paired is checked right after
    /// the handshake, with [`peer_fingerprint`], before anything else.
    pub fn server_endpoint(addr: SocketAddr, id: &Identity) -> Result<quinn::Endpoint, BoxError> {
        let provider = provider();
        let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(Check {
                provider,
                pinned: None,
            }))
            .with_single_cert(vec![id.cert.clone()], id.key())?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(transport());
        Ok(quinn::Endpoint::server(config, addr)?)
    }

    /// Which host certificate a client accepts.
    #[derive(Clone, Copy, Debug)]
    pub enum HostTrust {
        /// Only the paired host's.
        Pinned(Fingerprint),
        /// Any, for pairing: the pairing proof covers the certificate the
        /// client actually saw, so a stand-in can't pair.
        Pairing,
    }

    /// A client endpoint that shows this client's certificate.
    pub fn client_endpoint(
        bind: SocketAddr,
        id: &Identity,
        trust: HostTrust,
    ) -> Result<quinn::Endpoint, BoxError> {
        let provider = provider();
        let pinned = match trust {
            HostTrust::Pinned(f) => Some(f),
            HostTrust::Pairing => None,
        };
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Check { provider, pinned }))
            .with_client_auth_cert(vec![id.cert.clone()], id.key())?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(transport());
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(config);
        Ok(endpoint)
    }

    /// The fingerprint of the other side's certificate.
    pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
        let certs = conn
            .peer_identity()?
            .downcast::<Vec<CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(Fingerprint::of)
    }

    /// What a certificate on the other side must be: the pinned one, or any
    /// (pairing, and the host's view of clients). Self-signed certificates
    /// have no chain to check; the handshake signatures are still verified,
    /// so the peer holds the key.
    #[derive(Debug)]
    struct Check {
        provider: Arc<CryptoProvider>,
        pinned: Option<Fingerprint>,
    }

    impl Check {
        fn cert(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
            match self.pinned {
                Some(f) if Fingerprint::of(cert) != f => Err(rustls::Error::InvalidCertificate(
                    CertificateError::Other(rustls::OtherError(Arc::new(NotPaired))),
                )),
                _ => Ok(()),
            }
        }

        fn tls12(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn tls13(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// The host's certificate isn't the one paired with.
    #[derive(Debug, thiserror::Error)]
    #[error("the host's certificate isn't the one this client paired with")]
    pub struct NotPaired;

    impl ServerCertVerifier for Check {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            self.cert(end_entity)?;
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.tls12(message, cert, dss)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.tls13(message, cert, dss)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.schemes()
        }
    }

    impl ClientCertVerifier for Check {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _now: UnixTime,
        ) -> Result<ClientCertVerified, rustls::Error> {
            self.cert(end_entity)?;
            Ok(ClientCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.tls12(message, cert, dss)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.tls13(message, cert, dss)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.schemes()
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
    fn codec_is_the_clients_best_that_the_host_has() {
        use Codec::*;
        assert_eq!(
            Codec::pick(&[Hevc444, Hevc, H264], &[H264, Hevc]),
            Some(Hevc)
        );
        assert_eq!(Codec::pick(&[H264, Hevc444], &[Hevc444, H264]), Some(H264));
        assert_eq!(Codec::pick(&[Hevc444], &[H264]), None);
        assert_eq!(Codec::pick(&[], &[H264]), None);
    }

    #[test]
    fn clipboard_doesnt_echo() {
        let mut echo = ClipboardEcho::default();
        // A local copy is new once; seeing it again is not.
        assert!(echo.is_new("a"));
        assert!(!echo.is_new("a"));
        // Text from the other side, put on our clipboard: its change event
        // must not send it back.
        echo.noted("b");
        assert!(!echo.is_new("b"));
        // Copying the earlier text again is a real copy.
        assert!(echo.is_new("a"));
    }

    #[test]
    fn messages_roundtrip() {
        roundtrip(Hello {
            version: PROTOCOL_VERSION,
            intent: Intent::Connect {
                clipboard: true,
                codecs: vec![Codec::Hevc444, Codec::H264],
            },
        });
        roundtrip(ClientControl::Clipboard("héllo 👋\n世界".into()));
        roundtrip(HostControl::Clipboard(String::new()));
        roundtrip(HostControl::CursorImage(CursorImage {
            id: 7,
            width: 2,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![255, 0, 0, 255, 0, 0, 0, 0],
        }));
        roundtrip(HostControl::CursorShow(7));
        roundtrip(HostControl::CursorMoved { x: 0.25, y: 1.0 });
        roundtrip(HostControl::CursorInVideo);
        roundtrip(Hello {
            version: PROTOCOL_VERSION,
            intent: Intent::Pair {
                name: "desk".into(),
            },
        });
        roundtrip(PairMsg::Spake(vec![1, 2, 3]));
        roundtrip(PairMsg::Paired {
            name: "MacBook".into(),
            confirm: vec![9; 32],
        });
        roundtrip(Welcome {
            version: PROTOCOL_VERSION,
            width: 3456,
            height: 2234,
            codec: Codec::H264,
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
                delivered_us: 2,
                submitted_us: 2,
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
