//! The host's side of pairing, kept in plain files in one directory:
//!
//! - `host.cert.der`, `host.key.der`: this host's identity (key 0600);
//! - `clients`: one paired client per line, `<fingerprint> <name>`;
//! - `pairing`: while pairing is open, `<code> <expires (unix s)> <tries left>`.
//!
//! The daemon reads them on every connection, and `aether-agent pair` /
//! `unpair` change them, so the two need no channel between them. The
//! installed daemon uses a root-only directory; `serve` one in the user's
//! home.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aetherdesk_proto::trust::{Fingerprint, Identity, new_code, write_private};

/// The installed daemon's directory (root only).
pub const SYSTEM_DIR: &str = "/Library/Application Support/AetherDesk";

/// How long a pairing code works.
pub const PAIRING_TIME: Duration = Duration::from_secs(120);

/// Wrong codes before pairing closes.
pub const PAIRING_TRIES: u32 = 3;

/// A paired client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub fingerprint: Fingerprint,
    pub name: String,
}

/// An open pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    pub code: String,
    pub expires: u64,
    pub tries_left: u32,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The installed daemon's store.
    pub fn system() -> Self {
        Self::new(SYSTEM_DIR)
    }

    /// `serve`'s store, in the user's home, so it needs no root.
    pub fn serve() -> Result<Self, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME isn't set")?;
        Ok(Self::new(
            Path::new(&home).join("Library/Application Support/AetherDesk/serve"),
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn identity(&self) -> std::io::Result<Identity> {
        Identity::load_or_create(&self.dir, "host")
    }

    pub fn clients(&self) -> std::io::Result<Vec<Client>> {
        let text = match std::fs::read_to_string(self.dir.join("clients")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let (fp, name) = line.trim().split_once(' ')?;
                Some(Client {
                    fingerprint: fp.parse().ok()?,
                    name: name.trim().to_string(),
                })
            })
            .collect())
    }

    pub fn client(&self, fingerprint: &Fingerprint) -> std::io::Result<Option<Client>> {
        Ok(self
            .clients()?
            .into_iter()
            .find(|c| c.fingerprint == *fingerprint))
    }

    fn save_clients(&self, clients: &[Client]) -> std::io::Result<()> {
        let text: String = clients
            .iter()
            .map(|c| format!("{} {}\n", c.fingerprint, c.name))
            .collect();
        std::fs::create_dir_all(&self.dir)?;
        write_private(&self.dir.join("clients"), text.as_bytes())
    }

    /// Adds a client, replacing an earlier entry with the same certificate.
    pub fn add_client(&self, fingerprint: Fingerprint, name: &str) -> std::io::Result<()> {
        let mut clients = self.clients()?;
        clients.retain(|c| c.fingerprint != fingerprint);
        clients.push(Client {
            fingerprint,
            name: clean_name(name),
        });
        self.save_clients(&clients)
    }

    /// Removes the clients called `which`, or whose fingerprint starts with
    /// it; returns what was removed.
    pub fn remove_clients(&self, which: &str) -> std::io::Result<Vec<Client>> {
        let (gone, kept): (Vec<_>, Vec<_>) =
            self.clients()?.into_iter().partition(|c| c.matches(which));
        if !gone.is_empty() {
            self.save_clients(&kept)?;
        }
        Ok(gone)
    }

    /// Opens pairing with a fresh code for [`PAIRING_TIME`].
    pub fn open_pairing(&self) -> std::io::Result<Pairing> {
        let p = Pairing {
            code: new_code(),
            expires: now() + PAIRING_TIME.as_secs(),
            tries_left: PAIRING_TRIES,
        };
        self.save_pairing(&p)?;
        Ok(p)
    }

    fn save_pairing(&self, p: &Pairing) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_private(
            &self.dir.join("pairing"),
            format!("{} {} {}\n", p.code, p.expires, p.tries_left).as_bytes(),
        )
    }

    /// The open pairing, if there is one and it hasn't expired.
    pub fn pairing(&self) -> Option<Pairing> {
        let text = std::fs::read_to_string(self.dir.join("pairing")).ok()?;
        let mut parts = text.split_whitespace();
        let p = Pairing {
            code: parts.next()?.to_string(),
            expires: parts.next()?.parse().ok()?,
            tries_left: parts.next()?.parse().ok()?,
        };
        (p.expires > now() && p.tries_left > 0).then_some(p)
    }

    /// Counts a wrong code; closes pairing when no tries are left. Returns
    /// the tries left.
    pub fn pairing_failed(&self) -> u32 {
        let Some(mut p) = self.pairing() else {
            return 0;
        };
        p.tries_left -= 1;
        if p.tries_left == 0 {
            self.close_pairing();
        } else {
            let _ = self.save_pairing(&p);
        }
        p.tries_left
    }

    pub fn close_pairing(&self) {
        let _ = std::fs::remove_file(self.dir.join("pairing"));
    }
}

impl Client {
    fn matches(&self, which: &str) -> bool {
        self.name == which || (which.len() >= 4 && self.fingerprint.to_string().starts_with(which))
    }
}

/// A name for a list: one line, no surprises, not too long.
pub fn clean_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name: String = name.chars().take(64).collect();
    if name.is_empty() {
        "unnamed".into()
    } else {
        name
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("aetherdesk-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    #[test]
    fn clients_are_added_listed_and_removed() {
        let s = store("clients");
        assert!(s.clients().unwrap().is_empty());
        s.add_client(Fingerprint([1; 32]), "de\nsk").unwrap();
        s.add_client(Fingerprint([2; 32]), "laptop").unwrap();
        s.add_client(Fingerprint([1; 32]), "desk").unwrap(); // re-paired
        let names: Vec<_> = s.clients().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["laptop", "desk"]);
        assert!(s.client(&Fingerprint([2; 32])).unwrap().is_some());
        assert_eq!(s.remove_clients("laptop").unwrap().len(), 1);
        assert!(s.client(&Fingerprint([2; 32])).unwrap().is_none());
        assert_eq!(s.remove_clients("0101").unwrap().len(), 1); // by fingerprint
        assert!(s.clients().unwrap().is_empty());
        std::fs::remove_dir_all(s.dir()).unwrap();
    }

    #[test]
    fn pairing_closes_after_three_wrong_codes() {
        let s = store("pairing");
        assert!(s.pairing().is_none());
        let p = s.open_pairing().unwrap();
        assert_eq!(s.pairing(), Some(p));
        assert_eq!(s.pairing_failed(), 2);
        assert_eq!(s.pairing_failed(), 1);
        assert!(s.pairing().is_some());
        assert_eq!(s.pairing_failed(), 0);
        assert!(s.pairing().is_none());
        std::fs::remove_dir_all(s.dir()).unwrap();
    }

    #[test]
    fn expired_pairing_is_closed() {
        let s = store("expired");
        s.save_pairing(&Pairing {
            code: "123456".into(),
            expires: now() - 1,
            tries_left: 3,
        })
        .unwrap();
        assert!(s.pairing().is_none());
        std::fs::remove_dir_all(s.dir()).unwrap();
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  a\tb\nc  "), "a b c");
        assert_eq!(clean_name(""), "unnamed");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 64);
    }
}
