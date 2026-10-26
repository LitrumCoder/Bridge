//! Sans-IO core of Bridge: deterministic state machines that take events and time and
//! return actions. No sockets, threads or system clocks here (ADR-0001).

#![forbid(unsafe_code)]

pub mod clock;
mod feature;
mod node;
mod scheduler;
mod time;

pub use feature::{
    Feature, FeatureCx, FeatureError, FeatureInfo, FeatureRegistry, PeerDownReason, PeerId,
    PeerView, RegistryError, decode,
};
pub use node::{
    Action, ConnId, ConnectInfo, Core, CoreConfig, CoreEvent, CoreStats, HANDSHAKE_TIMEOUT,
    PeerInfo, Role,
};
pub use scheduler::{ChannelPlan, ChannelScheduler, ConnectionKind};
pub use time::Micros;

// TODO(M4a): layout geometry, InputRouter, KeyTracker.
// TODO(M8): device state machine and network profiles.
