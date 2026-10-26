//! Sans-IO core of Bridge: deterministic state machines that take events and time and
//! return actions. No sockets, threads or system clocks here.

#![forbid(unsafe_code)]

// TODO(M2): PeerSession, FeatureRegistry, ChannelScheduler, clock sync.
// TODO(M4a): layout geometry, InputRouter, KeyTracker.
// TODO(M8): device state machine and network profiles.
