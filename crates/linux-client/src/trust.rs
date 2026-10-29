//! This client's identity and the hosts it paired with, in
//! `$XDG_CONFIG_HOME/aetherdesk` (usually `~/.config/aetherdesk`):
//!
//! - `client.cert.der`, `client.key.der`: this client's certificate (key 0600);
//! - `hosts`: one paired host per line, `<fingerprint> <address> <name>`.

use std::path::{Path, PathBuf};

use aetherdesk_proto::trust::{Fingerprint, Identity, write_private};

/// A paired host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub fingerprint: Fingerprint,
    /// Where it was reached when pairing: IP or hostname, maybe with `:port`.
    pub address: String,
    pub name: String,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The user's store.
    pub fn user() -> Result<Self, String> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => Path::new(&std::env::var_os("HOME").ok_or("HOME isn't set")?).join(".config"),
        };
        Ok(Self {
            dir: base.join("aetherdesk"),
        })
    }

    #[cfg(test)]
    fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn identity(&self) -> Result<Identity, String> {
        Identity::load_or_create(&self.dir, "client")
            .map_err(|e| format!("this client's identity in {}: {e}", self.dir.display()))
    }

    pub fn hosts(&self) -> Result<Vec<Host>, String> {
        let path = self.dir.join("hosts");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut parts = line.trim().splitn(3, ' ');
                Some(Host {
                    fingerprint: parts.next()?.parse().ok()?,
                    address: parts.next()?.to_string(),
                    name: parts.next()?.trim().to_string(),
                })
            })
            .collect())
    }

    fn save(&self, hosts: &[Host]) -> Result<(), String> {
        let text: String = hosts
            .iter()
            .map(|h| format!("{} {} {}\n", h.fingerprint, h.address, h.name))
            .collect();
        std::fs::create_dir_all(&self.dir)
            .and_then(|()| write_private(&self.dir.join("hosts"), text.as_bytes()))
            .map_err(|e| format!("saving {}: {e}", self.dir.join("hosts").display()))
    }

    /// Adds a host, replacing an earlier entry with the same certificate or
    /// the same name.
    pub fn add(&self, host: Host) -> Result<(), String> {
        let mut hosts = self.hosts()?;
        hosts.retain(|h| h.fingerprint != host.fingerprint && h.name != host.name);
        hosts.push(host);
        self.save(&hosts)
    }

    /// Removes the hosts called `which` (or reached at that address).
    pub fn remove(&self, which: &str) -> Result<Vec<Host>, String> {
        let (gone, kept): (Vec<_>, Vec<_>) =
            self.hosts()?.into_iter().partition(|h| h.matches(which));
        if !gone.is_empty() {
            self.save(&kept)?;
        }
        Ok(gone)
    }

    /// The host to connect to: the one called (or reached at) `which`, or
    /// the only paired host when `which` is `None`.
    pub fn find(&self, which: Option<&str>) -> Result<Host, String> {
        let hosts = self.hosts()?;
        match which {
            Some(w) => hosts.into_iter().find(|h| h.matches(w)).ok_or(format!(
                "not paired with {w:?}. Pair first: open pairing on the Mac \
                 (scripts/mac.zsh pair), then run `aetherdesk pair <mac-address>`"
            )),
            None => match hosts.len() {
                0 => Err("not paired with any host yet. Open pairing on the Mac \
                     (scripts/mac.zsh pair), then run `aetherdesk pair <mac-address>`"
                    .into()),
                1 => Ok(hosts.into_iter().next().expect("one")),
                _ => Err(format!(
                    "paired with several hosts; say which: {}",
                    hosts
                        .iter()
                        .map(|h| h.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
        }
    }
}

impl Host {
    fn matches(&self, which: &str) -> bool {
        self.name.eq_ignore_ascii_case(which) || self.address == which
    }
}

/// This machine's name, to show in the host's list of clients.
pub fn client_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(b: u8, address: &str, name: &str) -> Host {
        Host {
            fingerprint: Fingerprint([b; 32]),
            address: address.into(),
            name: name.into(),
        }
    }

    #[test]
    fn hosts_are_added_found_and_removed() {
        let dir = std::env::temp_dir().join(format!("aetherdesk-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::at(dir.clone());
        assert!(s.find(None).unwrap_err().contains("not paired"));

        s.add(host(1, "192.0.2.10", "Office Mac")).unwrap();
        assert_eq!(s.find(None).unwrap().address, "192.0.2.10");
        assert_eq!(
            s.find(Some("office mac")).unwrap().fingerprint,
            Fingerprint([1; 32])
        );
        assert_eq!(s.find(Some("192.0.2.10")).unwrap().name, "Office Mac");

        s.add(host(2, "mini.tail.net:4501", "mini")).unwrap();
        assert!(s.find(None).unwrap_err().contains("several"));
        // Re-pairing the same Mac replaces it.
        s.add(host(3, "192.0.2.11", "Office Mac")).unwrap();
        assert_eq!(s.hosts().unwrap().len(), 2);

        assert_eq!(s.remove("mini").unwrap().len(), 1);
        assert_eq!(s.find(None).unwrap().fingerprint, Fingerprint([3; 32]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
