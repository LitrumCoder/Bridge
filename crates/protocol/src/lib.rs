//! Wire format of Bridge: message types, envelope, framing and version negotiation.
//!
//! This crate must stay free of OS, async runtime and transport dependencies
//! (enforced by `cargo xtask check-deps`).

#![forbid(unsafe_code)]

// TODO(M1): envelope, framing, bounded collections, core messages, version negotiation.
