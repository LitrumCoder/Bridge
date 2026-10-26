//! Trust store (ADR-0004): paired devices, their permissions and last known addresses,
//! persisted as `trust.toml` in the data directory.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bridge_protocol::{DeviceId, PermissionId};
use bridge_transport::TrustPolicy;
use serde::{Deserialize, Serialize};
use tracing::warn;

pub const ALL_PERMISSIONS: [PermissionId; 4] = [
    PermissionId::INPUT,
    PermissionId::FILES,
    PermissionId::CLIPBOARD,
    PermissionId::MIDI,
];
const MAX_ADDRS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct TrustedPeer {
    pub id: DeviceId,
    pub name: Option<String>,
    /// What we allow this peer to do to us.
    pub granted: Vec<PermissionId>,
    /// Last addresses the peer was reached at, newest first (manual add by IP lands here).
    pub addrs: Vec<SocketAddr>,
    pub added_at: u64,
    pub last_seen: Option<u64>,
}

impl TrustedPeer {
    /// Own devices get everything by default (ARCHITECTURE §9).
    pub fn own_device(id: DeviceId) -> Self {
        Self {
            id,
            name: None,
            granted: ALL_PERMISSIONS.to_vec(),
            addrs: Vec::new(),
            added_at: unix_now(),
            last_seen: None,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default, rename = "device")]
    devices: Vec<Record>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    granted: Vec<u16>,
    #[serde(default)]
    addrs: Vec<SocketAddr>,
    added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<u64>,
}

#[derive(Default)]
struct State {
    peers: HashMap<DeviceId, TrustedPeer>,
    pairing_until: Option<Instant>,
}

#[derive(Default)]
pub struct TrustStore {
    state: Mutex<State>,
    path: Option<PathBuf>,
}

impl TrustStore {
    /// A store that lives only in memory (tests).
    pub fn new(peers: impl IntoIterator<Item = TrustedPeer>) -> Arc<Self> {
        let peers = peers.into_iter().map(|p| (p.id, p)).collect();
        Arc::new(Self {
            state: Mutex::new(State {
                peers,
                pairing_until: None,
            }),
            path: None,
        })
    }

    /// Loads `path` (a missing file means an empty store) and saves back on changes.
    pub fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        let file: File = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e),
        };
        let mut peers = HashMap::new();
        for r in file.devices {
            let Some(id) = DeviceId::from_base32(&r.id) else {
                warn!(id = r.id, "skipping malformed trust record");
                continue;
            };
            peers.insert(
                id,
                TrustedPeer {
                    id,
                    name: r.name,
                    granted: r.granted.into_iter().map(PermissionId).collect(),
                    addrs: r.addrs,
                    added_at: r.added_at,
                    last_seen: r.last_seen,
                },
            );
        }
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                peers,
                pairing_until: None,
            }),
            path: Some(path.to_path_buf()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, id: &DeviceId) -> Option<TrustedPeer> {
        self.lock().peers.get(id).cloned()
    }

    pub fn all(&self) -> Vec<TrustedPeer> {
        let mut v: Vec<TrustedPeer> = self.lock().peers.values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    pub fn insert(&self, peer: TrustedPeer) {
        self.lock().peers.insert(peer.id, peer);
        self.save();
    }

    pub fn remove(&self, id: &DeviceId) -> bool {
        let removed = self.lock().peers.remove(id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    /// Applies `f` to an existing record and saves; returns false if unknown.
    pub fn update(&self, id: &DeviceId, f: impl FnOnce(&mut TrustedPeer)) -> bool {
        let found = self.lock().peers.get_mut(id).map(f).is_some();
        if found {
            self.save();
        }
        found
    }

    pub fn note_seen(&self, id: &DeviceId, name: &str, addr: Option<SocketAddr>) {
        self.update(id, |p| {
            p.name = Some(name.to_owned());
            p.last_seen = Some(unix_now());
            if let Some(a) = addr {
                p.addrs.retain(|x| *x != a);
                p.addrs.insert(0, a);
                p.addrs.truncate(MAX_ADDRS);
            }
        });
    }

    /// Lets unknown certificates through TLS until `until` (pairing window).
    pub fn set_pairing_until(&self, until: Option<Instant>) {
        self.lock().pairing_until = until;
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let file = File {
            devices: self
                .all()
                .into_iter()
                .map(|p| Record {
                    id: p.id.to_base32(),
                    name: p.name,
                    granted: p.granted.iter().map(|g| g.0).collect(),
                    addrs: p.addrs,
                    added_at: p.added_at,
                    last_seen: p.last_seen,
                })
                .collect(),
        };
        let result = toml::to_string_pretty(&file)
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|text| write_atomic(path, text.as_bytes()));
        if let Err(e) = result {
            warn!(path = %path.display(), "cannot save the trust store: {e}");
        }
    }
}

impl TrustPolicy for TrustStore {
    fn is_trusted(&self, id: &DeviceId) -> bool {
        self.lock().peers.contains_key(id)
    }

    fn pairing_open(&self) -> bool {
        self.lock()
            .pairing_until
            .is_some_and(|u| Instant::now() < u)
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("bridge-trust-{}", std::process::id()));
        let path = dir.join("trust.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let store = TrustStore::open(&path).expect("empty store");
        let id = DeviceId([4; 32]);
        store.insert(TrustedPeer::own_device(id));
        store.note_seen(
            &id,
            "Ноутбук",
            Some("192.168.1.7:47800".parse().expect("addr")),
        );
        assert!(store.update(&id, |p| p.granted = vec![PermissionId::FILES]));

        let again = TrustStore::open(&path).expect("reload");
        let p = again.get(&id).expect("persisted");
        assert_eq!(p.name.as_deref(), Some("Ноутбук"));
        assert_eq!(p.granted, vec![PermissionId::FILES]);
        assert_eq!(p.addrs.len(), 1);
        assert!(again.is_trusted(&id));
        assert!(again.remove(&id));
        assert!(!TrustStore::open(&path).expect("reload").is_trusted(&id));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn pairing_window_controls_tls_policy() {
        let store = TrustStore::new([]);
        assert!(!store.pairing_open());
        store.set_pairing_until(Some(Instant::now() + std::time::Duration::from_secs(60)));
        assert!(store.pairing_open());
        store.set_pairing_until(None);
        assert!(!store.pairing_open());
    }
}
