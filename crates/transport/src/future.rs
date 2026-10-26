//! Placeholders for transports planned after stage 1 (docs/ROADMAP.md). Nothing here
//! is implemented; each will be a `Transport` implementation selected by the core's
//! `ChannelScheduler`.
//!
//! - TODO(roadmap): Bluetooth LE — discovery, proximity, fallback channel for input, BLE-MIDI.
//! - TODO(roadmap): Wi-Fi Direct — links without a router.
//! - TODO(roadmap): cable (USB networking / Thunderbolt bridge).
//! - TODO(roadmap): relay through the internet with end-to-end TLS.
//! - TODO(roadmap): seamless switching between transports without dropping the session
//!   (`ClientHello::resume` already carries the session token).
