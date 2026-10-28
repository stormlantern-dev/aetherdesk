//! Who is who: certificates that last, their fingerprints, and the crypto of
//! pairing. Each machine has one self-signed certificate; pairing stores the
//! other side's [`Fingerprint`], and from then on TLS only lets that one in.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use ring::hmac;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

/// SHA-256 of a certificate's DER bytes: what each side pins.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        Self(bytes)
    }

    /// The first 8 bytes in hex, enough to tell machines apart in a list.
    pub fn short(&self) -> String {
        hex(&self.0[..8])
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

impl FromStr for Fingerprint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.len() != 64 {
            return Err(format!("a fingerprint is 64 hex digits, got {}", s.len()));
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                .map_err(|_| format!("not hex: {s}"))?;
        }
        Ok(Self(bytes))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// This machine's certificate and key.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Identity {
    /// A new self-signed certificate, made out to [`crate::quic::SERVER_NAME`]
    /// (clients use the same kind; the name doesn't matter for pinning).
    pub fn generate() -> Result<Self, rcgen::Error> {
        let c = rcgen::generate_simple_self_signed(vec![crate::quic::SERVER_NAME.into()])?;
        Ok(Self {
            cert: c.cert.der().clone(),
            key: c.signing_key.serialize_der(),
        })
    }

    /// Loads `<dir>/<stem>.cert.der` and `<dir>/<stem>.key.der`, or makes a
    /// new identity and saves it there. The key file is readable by the
    /// owner only.
    pub fn load_or_create(dir: &Path, stem: &str) -> std::io::Result<Self> {
        let cert_path = dir.join(format!("{stem}.cert.der"));
        let key_path = dir.join(format!("{stem}.key.der"));
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Self {
                cert: CertificateDer::from(cert),
                key,
            });
        }
        let id = Self::generate().map_err(std::io::Error::other)?;
        std::fs::create_dir_all(dir)?;
        write_private(&key_path, &id.key)?;
        write_private(&cert_path, id.cert.as_ref())?;
        Ok(id)
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert)
    }
}

/// Writes a file only its owner can read, replacing it in one step.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// A fresh 6-digit pairing code.
pub fn new_code() -> String {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    loop {
        let mut b = [0u8; 4];
        rng.fill(&mut b).expect("system randomness");
        let n = u32::from_le_bytes(b);
        // Reject the top sliver so every code is equally likely.
        if n < u32::MAX - u32::MAX % 1_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

/// A code as typed: digits only, so "123 456" and "123-456" work.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(char::is_ascii_digit).collect()
}

/// Which side of a pairing a confirmation comes from.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Client,
    Host,
}

/// One side of SPAKE2 with the pairing code.
pub struct Pake(Spake2<Ed25519Group>);

/// Starts SPAKE2; send the returned message to the other side.
pub fn start_pake(code: &str) -> (Pake, Vec<u8>) {
    let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(normalize_code(code).as_bytes()),
        &SpakeId::new(b"aetherdesk pairing v1"),
    );
    (Pake(state), msg)
}

impl Pake {
    /// The shared key, from the other side's message. Both sides get the same
    /// key only if they used the same code; that shows in [`PairKey::verify`].
    pub fn finish(self, theirs: &[u8]) -> Result<PairKey, String> {
        let key = self
            .0
            .finish(theirs)
            .map_err(|e| format!("pairing message: {e:?}"))?;
        Ok(PairKey(hmac::Key::new(hmac::HMAC_SHA256, &key)))
    }
}

pub struct PairKey(hmac::Key);

impl PairKey {
    /// Proof that `role` has the key, bound to both certificates of this TLS
    /// connection: a relay in the middle sees different certificates on each
    /// side, so its proofs don't match.
    pub fn confirm(&self, role: Role, client: &Fingerprint, host: &Fingerprint) -> Vec<u8> {
        hmac::sign(&self.0, &transcript(role, client, host))
            .as_ref()
            .to_vec()
    }

    /// Checks the other side's proof, in constant time.
    pub fn verify(&self, role: Role, client: &Fingerprint, host: &Fingerprint, mac: &[u8]) -> bool {
        hmac::verify(&self.0, &transcript(role, client, host), mac).is_ok()
    }
}

fn transcript(role: Role, client: &Fingerprint, host: &Fingerprint) -> Vec<u8> {
    let label: &[u8] = match role {
        Role::Client => b"aetherdesk client confirm",
        Role::Host => b"aetherdesk host confirm",
    };
    [label, &client.0, &host.0].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(b: u8) -> Fingerprint {
        Fingerprint([b; 32])
    }

    /// Both sides of a pairing with these codes; true if the host accepts
    /// the client and the client accepts the host.
    fn pair(client_code: &str, host_code: &str) -> bool {
        let (c, c_msg) = start_pake(client_code);
        let (h, h_msg) = start_pake(host_code);
        let (ck, hk) = (c.finish(&h_msg).unwrap(), h.finish(&c_msg).unwrap());
        let (cf, hf) = (fp(1), fp(2));
        let host_accepts = hk.verify(Role::Client, &cf, &hf, &ck.confirm(Role::Client, &cf, &hf));
        let client_accepts = ck.verify(Role::Host, &cf, &hf, &hk.confirm(Role::Host, &cf, &hf));
        host_accepts && client_accepts
    }

    #[test]
    fn same_code_pairs() {
        assert!(pair("123456", "123 456"));
    }

    #[test]
    fn wrong_code_fails() {
        assert!(!pair("123456", "123457"));
    }

    #[test]
    fn confirmation_is_bound_to_both_certificates() {
        let (c, c_msg) = start_pake("111111");
        let (h, h_msg) = start_pake("111111");
        let (ck, hk) = (c.finish(&h_msg).unwrap(), h.finish(&c_msg).unwrap());
        // A relay: the client saw host certificate 3, the host sees 2.
        let mac = ck.confirm(Role::Client, &fp(1), &fp(3));
        assert!(!hk.verify(Role::Client, &fp(1), &fp(2), &mac));
        // And a client proof doesn't pass as a host proof.
        let mac = ck.confirm(Role::Client, &fp(1), &fp(2));
        assert!(!ck.verify(Role::Host, &fp(1), &fp(2), &mac));
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..100 {
            let c = new_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|d| d.is_ascii_digit()));
        }
    }

    #[test]
    fn fingerprint_text_roundtrip() {
        let f = Fingerprint([0xab; 32]);
        assert_eq!(f.to_string().parse::<Fingerprint>().unwrap(), f);
        assert!("abc".parse::<Fingerprint>().is_err());
    }

    #[test]
    fn identity_is_kept() {
        let dir = std::env::temp_dir().join(format!("aetherdesk-id-{}", std::process::id()));
        let a = Identity::load_or_create(&dir, "host").unwrap();
        let b = Identity::load_or_create(&dir, "host").unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("host.key.der"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
