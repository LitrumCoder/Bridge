use std::net::SocketAddr;

use bridge_protocol::control::DeviceState;
use bridge_protocol::{DeviceId, Platform};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub device_id: DeviceId,
    pub name: String,
    pub listen: SocketAddr,
    pub state: DeviceState,
    pub peers: Vec<PeerStatus>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: DeviceId,
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub rtt_us: Option<u64>,
    pub clock_offset_us: Option<i64>,
}
