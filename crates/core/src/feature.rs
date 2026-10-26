//! Extension point for functionality (ARCHITECTURE §5): a new feature is a new type
//! implementing [`Feature`], registered once in the node builder.

use std::ops::RangeInclusive;

use bridge_protocol::control::{DeviceState, LayoutUpdate};
use bridge_protocol::{
    ChannelClass, DeviceId, EncodeError, FeatureDescriptor, FeatureId, PermissionId, Validate,
};
use serde::Serialize;

use crate::Micros;

pub type PeerId = DeviceId;

#[derive(Clone, Debug)]
pub struct FeatureInfo {
    pub id: FeatureId,
    pub name: &'static str,
    pub versions: RangeInclusive<u16>,
    /// Channel used by `FeatureCx::send`.
    pub channel: ChannelClass,
    /// Permission a peer needs from us before its messages reach the feature.
    pub permission: Option<PermissionId>,
}

impl FeatureInfo {
    pub fn descriptor(&self) -> FeatureDescriptor {
        FeatureDescriptor {
            id: self.id,
            min_version: *self.versions.start(),
            max_version: *self.versions.end(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerDownReason {
    /// Connection lost without a goodbye; the peer may come back.
    Lost,
    /// The peer announced `Left` or closed with `LEAVING`.
    Left,
    /// We or the peer revoked the pairing.
    Revoked,
    /// A newer connection to the same peer replaced this one.
    Replaced,
    /// Closed for another reason (protocol violation, shutdown).
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    /// The peer sent something it must not; the connection is closed.
    #[error("protocol violation: {0}")]
    Violation(String),
    #[error(transparent)]
    Decode(#[from] bridge_protocol::DecodeError),
    #[error("{0}")]
    Other(String),
}

/// A peer as seen by one feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PeerView {
    pub id: PeerId,
    /// Negotiated version of this feature with this peer.
    pub version: u16,
    /// Whether the peer granted us this feature's permission.
    pub allows_us: bool,
    /// What the peer last announced (`Paused` peers keep the connection but not the cursor).
    pub state: DeviceState,
    pub platform: bridge_protocol::Platform,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Outgoing {
    pub(crate) peer: PeerId,
    pub(crate) feature: FeatureId,
    pub(crate) kind: u16,
    pub(crate) payload: Vec<u8>,
    pub(crate) datagram: bool,
}

/// What a feature may do while handling an event.
pub struct FeatureCx<'a> {
    pub(crate) now: Micros,
    pub(crate) feature: FeatureId,
    pub(crate) peers: &'a [PeerView],
    pub(crate) out: &'a mut Vec<Outgoing>,
}

impl FeatureCx<'_> {
    pub fn now(&self) -> Micros {
        self.now
    }

    /// Active peers that negotiated this feature.
    pub fn peers(&self) -> &[PeerView] {
        self.peers
    }

    pub fn peer(&self, id: &PeerId) -> Option<&PeerView> {
        self.peers.iter().find(|p| p.id == *id)
    }

    /// Queues a message on the feature's channel. Messages to peers that are not active
    /// for this feature are dropped silently: the peer may have just gone away.
    pub fn send<M: Serialize>(
        &mut self,
        peer: PeerId,
        kind: u16,
        msg: &M,
    ) -> Result<(), EncodeError> {
        self.push(peer, kind, bridge_protocol::encode_payload(msg)?, false);
        Ok(())
    }

    /// Queues an already encoded payload (e.g. from a `define_messages!` enum).
    pub fn send_encoded(&mut self, peer: PeerId, kind: u16, payload: Vec<u8>) {
        self.push(peer, kind, payload, false);
    }

    /// Queues an unreliable datagram (`InputMotion`-style channels).
    pub fn send_datagram(&mut self, peer: PeerId, payload: Vec<u8>) {
        self.push(peer, 0, payload, true);
    }

    fn push(&mut self, peer: PeerId, kind: u16, payload: Vec<u8>, datagram: bool) {
        if self.peer(&peer).is_some() {
            self.out.push(Outgoing {
                peer,
                feature: self.feature,
                kind,
                payload,
                datagram,
            });
        }
    }
}

/// Sans-IO feature. All methods run on the core's thread and must not block.
pub trait Feature: Send + 'static {
    fn info(&self) -> FeatureInfo;

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, version: u16);

    fn on_peer_down(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, reason: PeerDownReason);

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError>;

    fn on_datagram(
        &mut self,
        _cx: &mut FeatureCx<'_>,
        _peer: PeerId,
        _payload: &[u8],
    ) -> Result<(), FeatureError> {
        Ok(())
    }

    fn on_device_state(&mut self, _cx: &mut FeatureCx<'_>, _state: DeviceState) {}

    /// The peer's permissions for us or its announced state changed; re-read
    /// `cx.peer(&peer)`.
    fn on_peer_update(&mut self, _cx: &mut FeatureCx<'_>, _peer: PeerId) {}

    fn on_tick(&mut self, _cx: &mut FeatureCx<'_>) {}

    /// The shared screen layout changed.
    fn on_layout(&mut self, _cx: &mut FeatureCx<'_>, _layout: &LayoutUpdate) {}

    /// An event posted from outside the network loop (an OS hook, a UI request).
    fn on_local(&mut self, _cx: &mut FeatureCx<'_>, _event: Box<dyn std::any::Any + Send>) {}
}

/// Decodes a message of type `T` for use inside `Feature::on_message`.
pub fn decode<T: serde::de::DeserializeOwned + Validate>(
    payload: &[u8],
) -> Result<T, FeatureError> {
    Ok(bridge_protocol::decode_payload(payload)?)
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("feature id {0} is reserved for the core")]
    Reserved(FeatureId),
    #[error("feature id {0} registered twice")]
    Duplicate(FeatureId),
    #[error("feature {0} has an empty version range")]
    EmptyVersions(FeatureId),
}

#[derive(Default)]
pub struct FeatureRegistry {
    entries: Vec<(FeatureInfo, Box<dyn Feature>)>,
}

impl FeatureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, feature: Box<dyn Feature>) -> Result<(), RegistryError> {
        let info = feature.info();
        if info.id == FeatureId::CORE {
            return Err(RegistryError::Reserved(info.id));
        }
        if info.versions.is_empty() {
            return Err(RegistryError::EmptyVersions(info.id));
        }
        if self.entries.iter().any(|(i, _)| i.id == info.id) {
            return Err(RegistryError::Duplicate(info.id));
        }
        self.entries.push((info, feature));
        self.entries.sort_by_key(|(i, _)| i.id);
        Ok(())
    }

    pub fn descriptors(&self) -> Vec<FeatureDescriptor> {
        self.entries.iter().map(|(i, _)| i.descriptor()).collect()
    }

    pub fn info(&self, id: FeatureId) -> Option<&FeatureInfo> {
        self.entries
            .iter()
            .find(|(i, _)| i.id == id)
            .map(|(i, _)| i)
    }

    pub(crate) fn get_mut(&mut self, id: FeatureId) -> Option<(&FeatureInfo, &mut dyn Feature)> {
        self.entries
            .iter_mut()
            .find(|(i, _)| i.id == id)
            .map(|(i, f)| (&*i, f.as_mut() as &mut dyn Feature))
    }

    pub(crate) fn ids(&self) -> Vec<FeatureId> {
        self.entries.iter().map(|(i, _)| i.id).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
