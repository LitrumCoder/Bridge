//! Test harness: in-memory UDP network with impairments and in-process multi-node
//! setups (ADR-0014). Dev-dependency only.

#![allow(clippy::panic)]

pub mod cluster;
pub mod netsim;

pub use cluster::{NodeSpec, TestNode, mutually_trusting, test_options};
pub use netsim::{LinkConfig, NetStats, Network, SimSocket};
