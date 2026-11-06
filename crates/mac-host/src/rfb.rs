//! A minimal RFB (VNC) client, enough to complete login at the macOS login
//! window through Screen Sharing (#72). macOS ignores synthetic input at the
//! login window from third-party processes, but accepts key events that come
//! in over Screen Sharing there.
//!
//! The daemon talks to Screen Sharing on `127.0.0.1:5900`, on the Mac
//! itself, using Apple's Diffie-Hellman security type (30). The exchange was
//! captured by the spike on macOS 27.2 (PR #107); the crypto here matches it
//! exactly. Type 30 authenticates with a Mac account and encrypts only that
//! credential block; the key events after it are plain RFB, and the server
//! isn't authenticated. Both are fine on localhost: nothing crosses the
//! network, and the account reached the daemon inside our own QUIC
//! connection.
//!
//! The exchange:
//!
//! 1. Server version (`RFB 003.889\n`), echoed back; the security types.
//! 2. We pick 30. The server sends its generator (2 bytes), key length (2
//!    bytes), prime and public value (key length bytes each).
//! 3. We pick a random private value of key length bytes, build the
//!    credential block (two 64-byte NUL-terminated fields, rest random),
//!    AES-128-ECB encrypt it with the MD5 of the shared secret, and send it
//!    followed by our public value.
//! 4. SecurityResult; ClientInit (shared); ServerInit.
//! 5. Key events, a down and an up per character, then Return.

use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

/// Apple's Diffie-Hellman security type, keyed with a Mac account.
pub const APPLE_DH: u8 = 30;

/// The X11 keysym for Return.
const RETURN: u32 = 0xff0d;

/// Longest account name or secret the credential block carries, in bytes
/// (64 per field, one kept for the NUL terminator).
pub const MAX_FIELD: usize = 63;

/// How this exchange can fail. Carries no account or secret.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Screen Sharing isn't reachable on {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("Screen Sharing speaks something other than RFB (got {0:?})")]
    NotRfb(String),
    #[error("Screen Sharing refused the connection: {0}")]
    ServerRefused(String),
    #[error("Screen Sharing doesn't offer account login (type {APPLE_DH})")]
    NoAccountLogin { offered: Vec<u8> },
    #[error("the account or secret is too long (max {MAX_FIELD} bytes each)")]
    FieldTooLong,
    #[error("Screen Sharing rejected the account")]
    AuthFailed,
    #[error("Screen Sharing didn't answer in time")]
    TimedOut,
    #[error("Screen Sharing protocol error: {0}")]
    Protocol(String),
    #[error("talking to Screen Sharing: {0}")]
    Io(#[from] std::io::Error),
}

/// Pads `x` to `len` big-endian bytes (left-zero-padded).
fn be_padded(x: &BigUint, len: usize) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; len.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

/// Builds and AES-128-ECB encrypts the credential block for Apple's DH
/// type, given the shared-secret-derived key. `account` and `secret` go in
/// two 64-byte NUL-terminated fields; the rest is the random bytes the
/// caller supplies (so the pure crypto stays testable). Returns the 128
/// ciphertext bytes. The plaintext is wiped before returning.
fn credential_block(
    key: &[u8; 16],
    account: &str,
    secret: &str,
    random: &[u8; 128],
) -> Result<[u8; 128], Error> {
    let mut block = *random;
    for (off, field) in [(0usize, account), (64, secret)] {
        let b = field.as_bytes();
        if b.len() > MAX_FIELD {
            block.zeroize();
            return Err(Error::FieldTooLong);
        }
        block[off..off + b.len()].copy_from_slice(b);
        block[off + b.len()] = 0;
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    for chunk in block.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    Ok(block)
}

/// The X11 keysym for a character, as RFB key events use. Latin-1 maps
/// straight; other Unicode is `0x0100_0000 + code point`.
fn keysym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// An RFB connection partway through the handshake: version done, a chosen
/// security type still to authenticate.
pub struct Handshake<S> {
    stream: S,
    offered: Vec<u8>,
}

/// Does the RFB version exchange on an already-connected stream and reads
/// the offered security types. Split from the socket so tests can drive it
/// over an in-memory pipe.
pub async fn start<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
) -> Result<Handshake<S>, Error> {
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    if !version.starts_with(b"RFB ") {
        return Err(Error::NotRfb(
            String::from_utf8_lossy(&version).trim_end().to_string(),
        ));
    }
    // Echo the server's version back (Apple offers 003.889, a 3.8 variant).
    stream.write_all(&version).await?;
    let n = stream.read_u8().await? as usize;
    if n == 0 {
        // A failure string follows (u32 length, then UTF-8).
        let len = stream.read_u32().await? as usize;
        let mut buf = vec![0u8; len.min(4096)];
        stream.read_exact(&mut buf).await?;
        return Err(Error::ServerRefused(
            String::from_utf8_lossy(&buf).into_owned(),
        ));
    }
    let mut offered = vec![0u8; n];
    stream.read_exact(&mut offered).await?;
    Ok(Handshake { stream, offered })
}

impl<S: AsyncRead + AsyncWrite + Unpin> Handshake<S> {
    /// Authenticates with Apple's DH type (30) using the Mac account, then
    /// does ClientInit/ServerInit. `random` supplies the bytes used for the
    /// private value and the credential-block padding; in production that's
    /// the system RNG. On success the connection is ready for key events.
    pub async fn log_in(
        mut self,
        account: &str,
        secret: &str,
        random: &(dyn Fn(&mut [u8]) + Send + Sync),
    ) -> Result<Session<S>, Error> {
        if !self.offered.contains(&APPLE_DH) {
            return Err(Error::NoAccountLogin {
                offered: self.offered.clone(),
            });
        }
        self.stream.write_u8(APPLE_DH).await?;

        // Generator (2 bytes), key length (2 bytes), prime and the server's
        // public value (key length bytes each).
        let mut gen_bytes = [0u8; 2];
        self.stream.read_exact(&mut gen_bytes).await?;
        let g = BigUint::from_bytes_be(&gen_bytes);
        let key_len = self.stream.read_u16().await? as usize;
        if key_len == 0 || key_len > 1024 {
            return Err(Error::Protocol(format!("odd key length {key_len}")));
        }
        let mut prime = vec![0u8; key_len];
        self.stream.read_exact(&mut prime).await?;
        let p = BigUint::from_bytes_be(&prime);
        let mut server_pub = vec![0u8; key_len];
        self.stream.read_exact(&mut server_pub).await?;
        let server_pub = BigUint::from_bytes_be(&server_pub);

        let mut priv_bytes = Zeroizing::new(vec![0u8; key_len]);
        random(&mut priv_bytes);
        let private = BigUint::from_bytes_be(&priv_bytes);
        let client_pub = g.modpow(&private, &p);
        let mut shared = Zeroizing::new(be_padded(&server_pub.modpow(&private, &p), key_len));
        let mut key = Md5::digest(&shared[..]);
        shared.zeroize();

        let mut random_pad = Zeroizing::new([0u8; 128]);
        random(&mut random_pad[..]);
        let key_arr: [u8; 16] = key.into();
        let block = credential_block(&key_arr, account, secret, &random_pad);
        key.zeroize();
        let block = block?;

        self.stream.write_all(&block).await?;
        self.stream
            .write_all(&be_padded(&client_pub, key_len))
            .await?;
        self.stream.flush().await?;

        // SecurityResult: 0 is success, anything else a failure (a reason
        // string may follow, which we don't need).
        match self.stream.read_u32().await? {
            0 => {}
            _ => return Err(Error::AuthFailed),
        }

        // ClientInit: share the session (don't disconnect other viewers).
        self.stream.write_u8(1).await?;
        self.stream.flush().await?;
        // ServerInit: width, height, 16-byte pixel format, name (u32 length
        // then bytes). We don't use any of it; read past it.
        let _width = self.stream.read_u16().await?;
        let _height = self.stream.read_u16().await?;
        let mut pixel_format = [0u8; 16];
        self.stream.read_exact(&mut pixel_format).await?;
        let name_len = self.stream.read_u32().await? as usize;
        let mut name = vec![0u8; name_len.min(4096)];
        self.stream.read_exact(&mut name).await?;
        Ok(Session {
            stream: self.stream,
        })
    }
}

/// An authenticated RFB session: key events can be sent.
pub struct Session<S> {
    stream: S,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    /// Sends `text` as key events (a down then an up per character) and then
    /// Return, which is what the login window waits for. The text's own
    /// bytes never leave this process.
    pub async fn type_and_submit(&mut self, text: &str) -> Result<(), Error> {
        for c in text.chars() {
            self.key(keysym(c)).await?;
        }
        self.key(RETURN).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// One key: a KeyEvent down, then up (message type 4).
    async fn key(&mut self, sym: u32) -> Result<(), Error> {
        for down in [1u8, 0] {
            let mut msg = [0u8; 8];
            msg[0] = 4;
            msg[1] = down;
            msg[4..8].copy_from_slice(&sym.to_be_bytes());
            self.stream.write_all(&msg).await?;
        }
        Ok(())
    }
}

/// Caps how often login may be attempted on one connection, so an
/// already-paired client can't use the feature to guess the Mac password at
/// speed. Not a clock of its own: the caller passes the current instant, so
/// it's easy to test.
pub struct RateLimiter {
    window: Duration,
    max: usize,
    hits: std::collections::VecDeque<std::time::Instant>,
}

impl RateLimiter {
    /// At most `max` attempts within any `window`.
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            window,
            max,
            hits: std::collections::VecDeque::new(),
        }
    }

    /// Records an attempt at `now`. `Ok(())` to go ahead; `Err(secs)` when
    /// the cap is reached, with the whole seconds until the oldest attempt
    /// falls out of the window (at least 1).
    pub fn check(&mut self, now: std::time::Instant) -> Result<(), u32> {
        while let Some(&front) = self.hits.front() {
            if now.duration_since(front) >= self.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        if self.hits.len() >= self.max {
            let oldest = *self.hits.front().expect("len >= max >= 1");
            let wait = self.window - now.duration_since(oldest);
            return Err(wait.as_secs().max(1) as u32);
        }
        self.hits.push_back(now);
        Ok(())
    }
}

/// Times out an RFB step, mapping an elapsed deadline onto [`Error::TimedOut`].
pub async fn with_timeout<T>(
    dur: Duration,
    f: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match tokio::time::timeout(dur, f).await {
        Ok(r) => r,
        Err(_) => Err(Error::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential block must match the spike byte for byte: the account
    /// and secret in two NUL-terminated 64-byte fields, AES-128-ECB with the
    /// MD5-derived key. We check by decrypting and reading the fields back.
    #[test]
    fn credential_block_round_trips() {
        use aes::cipher::BlockDecrypt;
        let key = [7u8; 16];
        let random = [0xABu8; 128];
        let block = credential_block(&key, "alice", "s3cret-ü", &random).unwrap();

        // Fields that weren't overwritten keep the random padding.
        assert_eq!(&block[..] as &[u8], &block[..]);
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        let mut plain = block;
        for chunk in plain.chunks_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        }
        // Account field.
        assert_eq!(&plain[..5], b"alice");
        assert_eq!(plain[5], 0);
        // Padding past the NUL is the supplied random, untouched.
        assert_eq!(plain[6], 0xAB);
        // Secret field at offset 64.
        assert_eq!(&plain[64..64 + "s3cret-ü".len()], "s3cret-ü".as_bytes());
        assert_eq!(plain[64 + "s3cret-ü".len()], 0);
    }

    #[test]
    fn a_field_at_the_limit_is_accepted_over_it_rejected() {
        let key = [0u8; 16];
        let random = [0u8; 128];
        assert!(credential_block(&key, &"a".repeat(MAX_FIELD), "x", &random).is_ok());
        assert!(matches!(
            credential_block(&key, &"a".repeat(MAX_FIELD + 1), "x", &random),
            Err(Error::FieldTooLong)
        ));
    }

    #[test]
    fn keysyms_match_x11() {
        assert_eq!(keysym('A'), 0x41);
        assert_eq!(keysym(' '), 0x20);
        assert_eq!(keysym('ü'), 0xFC); // Latin-1
        assert_eq!(keysym('€'), 0x0100_0000 + 0x20AC); // beyond Latin-1
        assert_eq!(RETURN, 0xff0d);
    }

    #[test]
    fn be_padded_pads_and_preserves() {
        let x = BigUint::from(0x0102u32);
        assert_eq!(be_padded(&x, 4), vec![0, 0, 1, 2]);
        assert_eq!(be_padded(&x, 2), vec![1, 2]);
        // Never truncates a value that's already longer.
        assert_eq!(be_padded(&x, 1), vec![1, 2]);
    }

    /// A fake Screen Sharing that does the Apple DH exchange and checks the
    /// client's account, then reports the typed keysyms. Uses a small prime
    /// (the maths is identical, just faster); g = 2.
    async fn fake_server(
        mut s: tokio::io::DuplexStream,
        offered: &[u8],
        accept: bool,
    ) -> (String, Vec<u32>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Curve25519's prime, 32 bytes: a convenient prime for the test.
        let p = BigUint::parse_bytes(
            b"7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffed",
            16,
        )
        .unwrap();
        let g = BigUint::from(2u32);
        let key_len = 32usize;

        s.write_all(b"RFB 003.889\n").await.unwrap();
        let mut echo = [0u8; 12];
        s.read_exact(&mut echo).await.unwrap();
        s.write_u8(offered.len() as u8).await.unwrap();
        s.write_all(offered).await.unwrap();

        let chosen = s.read_u8().await.unwrap();
        assert_eq!(chosen, APPLE_DH);

        let server_priv = BigUint::from(0x1234_5678u32);
        let server_pub = g.modpow(&server_priv, &p);
        s.write_all(&[0, 2]).await.unwrap(); // generator = 2
        s.write_u16(key_len as u16).await.unwrap();
        s.write_all(&be_padded(&p, key_len)).await.unwrap();
        s.write_all(&be_padded(&server_pub, key_len)).await.unwrap();

        let mut block = [0u8; 128];
        s.read_exact(&mut block).await.unwrap();
        let mut client_pub = vec![0u8; key_len];
        s.read_exact(&mut client_pub).await.unwrap();
        let client_pub = BigUint::from_bytes_be(&client_pub);
        let shared = client_pub.modpow(&server_priv, &p);
        let key = Md5::digest(be_padded(&shared, key_len));

        use aes::cipher::BlockDecrypt;
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        for chunk in block.chunks_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        }
        let account = {
            let end = block[..64].iter().position(|&b| b == 0).unwrap_or(64);
            String::from_utf8_lossy(&block[..end]).into_owned()
        };

        s.write_u32(if accept { 0 } else { 1 }).await.unwrap();
        if !accept {
            return (account, Vec::new());
        }
        let _client_init = s.read_u8().await.unwrap();
        s.write_u16(100).await.unwrap(); // width
        s.write_u16(100).await.unwrap(); // height
        s.write_all(&[0u8; 16]).await.unwrap(); // pixel format
        s.write_u32(3).await.unwrap(); // name length
        s.write_all(b"Mac").await.unwrap();

        // Collect keysyms until the stream closes. Each key is a down (8
        // bytes) then an up (8 bytes); record one keysym per down.
        let mut syms = Vec::new();
        let mut msg = [0u8; 8];
        while s.read_exact(&mut msg).await.is_ok() {
            if msg[0] == 4 && msg[1] == 1 {
                syms.push(u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]));
            }
        }
        (account, syms)
    }

    #[tokio::test]
    async fn logs_in_against_a_fake_screen_sharing() {
        let (client, server) = tokio::io::duplex(8192);
        let server = tokio::spawn(fake_server(server, &[APPLE_DH, 36], true));
        // Deterministic "randomness" for the test.
        let random = |b: &mut [u8]| {
            for (i, x) in b.iter_mut().enumerate() {
                *x = (i as u8).wrapping_mul(31).wrapping_add(1);
            }
        };
        let hs = start(client).await.unwrap();
        assert_eq!(hs.offered, vec![APPLE_DH, 36]);
        let mut session = hs.log_in("alice", "pw-ü", &random).await.unwrap();
        session.type_and_submit("Hi").await.unwrap();
        drop(session);
        let (account, syms) = server.await.unwrap();
        assert_eq!(account, "alice");
        assert_eq!(syms, vec![keysym('H'), keysym('i'), RETURN]);
    }

    #[tokio::test]
    async fn without_type_30_it_says_so() {
        let (client, server) = tokio::io::duplex(8192);
        // The server offers only other types; log_in never reaches it.
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = server;
            s.write_all(b"RFB 003.889\n").await.unwrap();
            let mut echo = [0u8; 12];
            s.read_exact(&mut echo).await.unwrap();
            s.write_u8(2).await.unwrap();
            s.write_all(&[33, 36]).await.unwrap();
        });
        let hs = start(client).await.unwrap();
        let random = |_: &mut [u8]| {};
        match hs.log_in("alice", "pw", &random).await {
            Err(Error::NoAccountLogin { offered }) => assert_eq!(offered, vec![33, 36]),
            Err(e) => panic!("expected NoAccountLogin, got {e:?}"),
            Ok(_) => panic!("expected NoAccountLogin, logged in"),
        }
    }

    #[tokio::test]
    async fn a_rejected_account_is_reported() {
        let (client, server) = tokio::io::duplex(8192);
        tokio::spawn(fake_server(server, &[APPLE_DH], false));
        let random = |b: &mut [u8]| b.fill(2);
        let hs = start(client).await.unwrap();
        assert!(matches!(
            hs.log_in("alice", "wrong", &random).await,
            Err(Error::AuthFailed)
        ));
    }

    #[test]
    fn rate_limiter_caps_a_burst_then_recovers() {
        use std::time::Instant;
        let mut rl = RateLimiter::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        // Three in quick succession are fine.
        assert!(rl.check(t0).is_ok());
        assert!(rl.check(t0 + Duration::from_secs(1)).is_ok());
        assert!(rl.check(t0 + Duration::from_secs(2)).is_ok());
        // The fourth within the window is refused, with the wait to the
        // first one ageing out (~58 s).
        match rl.check(t0 + Duration::from_secs(2)) {
            Err(secs) => assert!((57..=60).contains(&secs), "{secs}"),
            Ok(()) => panic!("a fourth attempt should be refused"),
        }
        // Once the first falls out of the window, one more is allowed.
        assert!(rl.check(t0 + Duration::from_secs(61)).is_ok());
    }

    #[tokio::test]
    async fn a_non_rfb_greeting_is_rejected() {
        let (client, server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut s = server;
            s.write_all(b"HTTP/1.1 200").await.unwrap();
        });
        assert!(matches!(start(client).await, Err(Error::NotRfb(_))));
    }
}
