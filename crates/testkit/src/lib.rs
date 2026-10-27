//! Test harness: in-memory UDP network with impairments and in-process multi-node
//! setups (ADR-0014). Dev-dependency only.

#![allow(clippy::panic)]

pub mod cluster;
pub mod loopback;
pub mod netsim;

pub use cluster::{NodeSpec, TestNode, mutually_trusting, strangers, test_options};
pub use netsim::{LinkConfig, NetStats, Network, SimSocket};

/// The simulator delivers packets on tokio timers. Windows ticks every 15.6 ms unless
/// asked otherwise, which the bulk connection's controller would read as queuing
/// delay; this asks for 1 ms for the rest of the process.
pub fn fine_timers() {
    #[cfg(windows)]
    // SAFETY: plain call; the period is restored when the process exits.
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
}
