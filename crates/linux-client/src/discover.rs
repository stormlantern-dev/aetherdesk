//! Finding Macs on the LAN over mDNS (see [`aetherdesk_proto::discovery`]).
//!
//! A [`Finder`] browses in the background for as long as it lives and keeps
//! the latest address of every host it heard, keyed by certificate
//! fingerprint. What it finds is only a hint: the connection still pins the
//! paired certificate, so a false announcement leads nowhere.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aetherdesk_proto::discovery::{SERVICE_FQDN, TXT_FINGERPRINT};
use aetherdesk_proto::trust::Fingerprint;
use mdns_sd::{ServiceDaemon, ServiceEvent};

/// A host heard on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The instance name it announced (on a Mac: its computer name).
    pub name: String,
    pub fingerprint: Fingerprint,
    pub address: SocketAddr,
    /// The DNS-SD full name, to forget it when it says goodbye.
    fullname: String,
}

pub struct Finder {
    daemon: ServiceDaemon,
    seen: Arc<Mutex<HashMap<Fingerprint, Found>>>,
}

impl Finder {
    /// Starts browsing. Fails when there's no network to multicast on.
    pub fn start() -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS: {e}"))?;
        let events = daemon
            .browse(SERVICE_FQDN)
            .map_err(|e| format!("mDNS browse: {e}"))?;
        let seen = Arc::new(Mutex::new(HashMap::new()));
        let table = seen.clone();
        std::thread::Builder::new()
            .name("mdns".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    let mut table = table.lock().unwrap();
                    match event {
                        ServiceEvent::ServiceResolved(s) => {
                            let fp = s.txt_properties.get_property_val_str(TXT_FINGERPRINT);
                            let ip = best_address(s.addresses.iter().map(|a| a.to_ip_addr()));
                            if let (Some(fp), Some(ip)) = (fp.and_then(|f| f.parse().ok()), ip) {
                                table.insert(
                                    fp,
                                    Found {
                                        name: instance_name(&s.fullname, &s.ty_domain),
                                        fingerprint: fp,
                                        address: SocketAddr::new(ip, s.port),
                                        fullname: s.fullname.clone(),
                                    },
                                );
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            table.retain(|_, f| f.fullname != fullname);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| format!("mDNS thread: {e}"))?;
        Ok(Self { daemon, seen })
    }

    /// Where the host with this certificate is, if we heard it.
    pub fn get(&self, fingerprint: &Fingerprint) -> Option<Found> {
        self.seen.lock().unwrap().get(fingerprint).cloned()
    }

    /// Like [`get`](Self::get), waiting up to `wait` for an answer.
    pub async fn wait_for(&self, fingerprint: &Fingerprint, wait: Duration) -> Option<Found> {
        let until = Instant::now() + wait;
        loop {
            if let Some(f) = self.get(fingerprint) {
                return Some(f);
            }
            if Instant::now() >= until {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Every host heard so far, by name.
    pub fn all(&self) -> Vec<Found> {
        let mut all: Vec<_> = self.seen.lock().unwrap().values().cloned().collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all
    }

    /// Browses for `wait`, or until `done` says enough; returns all found.
    pub fn collect(&self, wait: Duration, done: impl Fn(&[Found]) -> bool) -> Vec<Found> {
        let until = Instant::now() + wait;
        loop {
            let all = self.all();
            if done(&all) || Instant::now() >= until {
                return all;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Finder {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// The address to dial among those announced: routable IPv4 first, then
/// routable IPv6, then whatever is left. Link-local IPv6 is skipped: it needs
/// an interface scope we don't carry.
fn best_address(addrs: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    let rank = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(0),
        IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unicast_link_local() => Some(1),
        IpAddr::V4(_) => Some(2),
        IpAddr::V6(v6) if v6.is_loopback() => Some(3),
        IpAddr::V6(_) => None,
    };
    addrs
        .filter_map(|ip| rank(&ip).map(|r| (r, ip)))
        .min()
        .map(|(_, ip)| ip)
}

/// `My Mac._aetherdesk._udp.local.` → `My Mac`, undoing DNS-SD escapes.
fn instance_name(fullname: &str, ty_domain: &str) -> String {
    let raw = fullname
        .strip_suffix(ty_domain)
        .and_then(|n| n.strip_suffix('.'))
        .unwrap_or(fullname);
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routable_ipv4_wins() {
        let ips: Vec<IpAddr> = vec![
            "fe80::1".parse().unwrap(),
            "2001:db8::5".parse().unwrap(),
            "169.254.3.4".parse().unwrap(),
            "192.0.2.10".parse().unwrap(),
        ];
        assert_eq!(
            best_address(ips.into_iter()),
            Some("192.0.2.10".parse().unwrap())
        );
    }

    #[test]
    fn link_local_ipv6_alone_is_useless() {
        let ips: Vec<IpAddr> = vec!["fe80::1".parse().unwrap()];
        assert_eq!(best_address(ips.into_iter()), None);
        let ips: Vec<IpAddr> = vec!["fe80::1".parse().unwrap(), "2001:db8::5".parse().unwrap()];
        assert_eq!(
            best_address(ips.into_iter()),
            Some("2001:db8::5".parse().unwrap())
        );
    }

    #[test]
    fn instance_names_are_unescaped() {
        assert_eq!(
            instance_name(
                "Office Mac._aetherdesk._udp.local.",
                "_aetherdesk._udp.local."
            ),
            "Office Mac"
        );
        assert_eq!(
            instance_name(
                "v1\\.2 Mac._aetherdesk._udp.local.",
                "_aetherdesk._udp.local."
            ),
            "v1.2 Mac"
        );
    }
}
