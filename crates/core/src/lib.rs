//! Sans-IO core of Bridge: deterministic state machines that take events and time and
//! return actions. No sockets, threads or system clocks here (ADR-0001).

#![forbid(unsafe_code)]

pub mod clock;
mod feature;
pub mod input;
mod layout;
mod node;
pub mod pairing;
mod scheduler;
mod time;

pub use layout::LocalScreen;
pub use pairing::{Entropy, PAIRING_WINDOW, PairingError, PairingSecret};

pub use feature::{
    Feature, FeatureCx, FeatureError, FeatureInfo, FeatureRegistry, PeerDownReason, PeerId,
    PeerView, RegistryError, decode,
};
pub use node::{
    Action, ConnId, ConnectInfo, Core, CoreConfig, CoreEvent, CoreStats, HANDSHAKE_TIMEOUT,
    PAIRING_HANDSHAKE_TIMEOUT, PeerInfo, Role,
};
pub use scheduler::{ChannelPlan, ChannelScheduler, ConnectionKind};
pub use time::Micros;

// TODO(M4a): layout geometry, InputRouter, KeyTracker.
// TODO(M8): device state machine and network profiles.
