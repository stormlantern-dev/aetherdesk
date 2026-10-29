//! Finding a host on the LAN with mDNS / DNS-SD (Bonjour).
//!
//! The host announces [`SERVICE`] on its QUIC port, with TXT entries:
//!
//! - `fp`: the SHA-256 [`Fingerprint`] of its certificate, 64 hex digits;
//! - `v`: its [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
//!
//! A client recognises its paired host by `fp`, never by the instance name:
//! anyone on the LAN can announce any name. Discovery only says where to
//! knock; mutual TLS still decides who gets in.

use crate::trust::Fingerprint;

/// The DNS-SD service type.
pub const SERVICE: &str = "_aetherdesk._udp";

/// The same, as a fully qualified name for resolvers that want one.
pub const SERVICE_FQDN: &str = "_aetherdesk._udp.local.";

/// TXT key for the certificate fingerprint.
pub const TXT_FINGERPRINT: &str = "fp";

/// TXT key for the protocol version.
pub const TXT_VERSION: &str = "v";

/// The TXT entries a host announces.
pub fn txt_entries(fingerprint: &Fingerprint) -> [(&'static str, String); 2] {
    [
        (TXT_FINGERPRINT, fingerprint.to_string()),
        (TXT_VERSION, crate::PROTOCOL_VERSION.to_string()),
    ]
}

/// The TXT entries in DNS wire format (each `key=value` prefixed with its
/// length), as `DNSServiceRegister` takes them.
pub fn txt_record(fingerprint: &Fingerprint) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in txt_entries(fingerprint) {
        let entry = format!("{key}={value}");
        let len = u8::try_from(entry.len()).expect("TXT entries are short");
        out.push(len);
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_record_is_length_prefixed() {
        let fp = Fingerprint([0xab; 32]);
        let txt = txt_record(&fp);
        let first = 3 + 64;
        assert_eq!(txt[0] as usize, first);
        assert_eq!(&txt[1..4], b"fp=");
        assert_eq!(
            std::str::from_utf8(&txt[4..1 + first]).unwrap(),
            fp.to_string()
        );
        let v = format!("v={}", crate::PROTOCOL_VERSION);
        assert_eq!(txt[1 + first] as usize, v.len());
        assert_eq!(&txt[2 + first..], v.as_bytes());
    }
}
