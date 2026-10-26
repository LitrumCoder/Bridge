//! Device discovery, kept separate from transport so that the discovery channel and the
//! data channel can change independently (ARCHITECTURE §9).

mod mdns;
mod memory;

use std::collections::HashMap;
use std::net::SocketAddr;

use bridge_protocol::DeviceId;
use tokio::sync::mpsc;

pub use mdns::MdnsDiscovery;
pub use memory::{MemoryBus, MemoryDiscovery};

pub const SERVICE_TYPE: &str = "_bridge._udp.local.";
pub const PROTO_VERSION_TXT: &str = "1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdState {
    Connected,
    Paused,
    Left,
}

impl AdState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Paused => "paused",
            Self::Left => "left",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "connected" => Self::Connected,
            "paused" => Self::Paused,
            "left" => Self::Left,
            _ => return None,
        })
    }
}

/// What this device announces.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Advertisement {
    pub id: DeviceId,
    /// `None` hides the name from the network (privacy setting).
    pub name: Option<String>,
    pub port: u16,
    pub state: AdState,
}

/// Another device seen on the network. Everything here is a claim, not an identity:
/// the certificate checked during the TLS handshake is what counts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Discovered {
    pub id: DeviceId,
    pub name: Option<String>,
    pub addrs: Vec<SocketAddr>,
    pub state: AdState,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiscoveryEvent {
    Found(Discovered),
    Lost(DeviceId),
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mdns: {0}")]
    Mdns(String),
}

pub trait Discovery: Send + Sync + 'static {
    /// Starts or updates our announcement.
    fn advertise(&self, ad: &Advertisement) -> Result<(), DiscoveryError>;
    /// Stops announcing (leaving the group).
    fn withdraw(&self);
    /// Events about other devices; each call gets its own stream.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<DiscoveryEvent>;
}

const MAX_NAME_LEN: usize = 64;

pub(crate) fn txt_properties(ad: &Advertisement) -> HashMap<String, String> {
    let mut txt = HashMap::new();
    txt.insert("id".to_owned(), ad.id.to_base32());
    txt.insert("proto".to_owned(), PROTO_VERSION_TXT.to_owned());
    txt.insert("state".to_owned(), ad.state.as_str().to_owned());
    if let Some(name) = &ad.name {
        let name: String = name.chars().take(MAX_NAME_LEN).collect();
        txt.insert("name".to_owned(), name);
    }
    txt
}

/// Parses an announcement from TXT values; anything malformed is ignored.
pub(crate) fn parse_txt(
    get: impl Fn(&str) -> Option<String>,
    addrs: Vec<SocketAddr>,
) -> Option<Discovered> {
    if get("proto")? != PROTO_VERSION_TXT {
        return None;
    }
    let id = DeviceId::from_base32(&get("id")?)?;
    let state = get("state")
        .and_then(|s| AdState::parse(&s))
        .unwrap_or(AdState::Connected);
    let name = get("name")
        .map(|n| {
            n.chars()
                .filter(|c| !c.is_control())
                .take(MAX_NAME_LEN)
                .collect::<String>()
        })
        .filter(|n| !n.is_empty());
    Some(Discovered {
        id,
        name,
        addrs,
        state,
    })
}

/// mDNS instance names are the short fingerprint, so names never leak through them.
pub(crate) fn instance_name(id: &DeviceId) -> String {
    format!("bridge-{}", id.fingerprint())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_roundtrip_and_hidden_name() {
        let ad = Advertisement {
            id: DeviceId([3; 32]),
            name: Some("Ноутбук\u{7}".into()),
            port: 47800,
            state: AdState::Paused,
        };
        let txt = txt_properties(&ad);
        let addr: SocketAddr = "192.168.1.5:47800".parse().unwrap();
        let d = parse_txt(|k| txt.get(k).cloned(), vec![addr]).unwrap();
        assert_eq!(d.id, ad.id);
        assert_eq!(
            d.name.as_deref(),
            Some("Ноутбук"),
            "control characters are stripped"
        );
        assert_eq!(d.state, AdState::Paused);
        assert_eq!(d.addrs, vec![addr]);

        let hidden = Advertisement { name: None, ..ad };
        let txt = txt_properties(&hidden);
        assert!(!txt.contains_key("name"));
        assert_eq!(
            parse_txt(|k| txt.get(k).cloned(), vec![]).unwrap().name,
            None
        );
    }

    #[test]
    fn malformed_or_foreign_announcements_are_ignored() {
        let none = |_: &str| None;
        assert!(parse_txt(none, vec![]).is_none());
        let other_proto = |k: &str| match k {
            "proto" => Some("2".to_owned()),
            "id" => Some(DeviceId([1; 32]).to_base32()),
            _ => None,
        };
        assert!(parse_txt(other_proto, vec![]).is_none());
        let bad_id = |k: &str| match k {
            "proto" => Some("1".to_owned()),
            "id" => Some("not-an-id".to_owned()),
            _ => None,
        };
        assert!(parse_txt(bad_id, vec![]).is_none());
        assert!(instance_name(&DeviceId([0; 32])).starts_with("bridge-"));
    }
}
