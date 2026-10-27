//! Wire format of Bridge: message types, envelope, framing and version negotiation
//! (ARCHITECTURE §3, ADR-0003).
//!
//! This crate must stay free of OS, async runtime and transport dependencies
//! (enforced by `cargo xtask check-deps`). Everything decoded here comes from an
//! untrusted peer: decoders never panic, check lengths before allocating and
//! validate values.

#![forbid(unsafe_code)]

mod bounded;
mod channel;
pub mod clipboard;
pub mod control;
mod envelope;
mod error;
pub mod files;
mod frame;
mod ids;
pub mod input;
mod message;
#[cfg(any(test, feature = "proptest"))]
pub mod strategy;
mod version;

pub use bounded::{BoundedBytes, BoundedString, BoundedVec};
pub use channel::{ChannelClass, CloseCode};
pub use envelope::{DATAGRAM_FORMAT, Envelope, EnvelopeRef, decode_datagram, encode_datagram};
pub use error::{DecodeError, EncodeError};
pub use frame::{FrameDecoder, encode_frame};
pub use ids::{
    DeviceId, FeatureId, PermissionId, Platform, SessionToken, base32_decode, base32_encode,
};
pub use message::{Validate, decode_payload, encode_payload};
pub use version::{
    FeatureDescriptor, IncompatibleVersion, NegotiatedFeature, PROTOCOL_VERSION, ProtocolVersion,
    check_compatibility, negotiate_features,
};
