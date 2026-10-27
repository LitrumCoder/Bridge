use std::net::SocketAddr;

use bridge_protocol::control::{DeviceState, LayoutUpdate};
use bridge_protocol::{DeviceId, Platform};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub device_id: DeviceId,
    pub name: String,
    pub listen: SocketAddr,
    pub state: DeviceState,
    pub peers: Vec<PeerStatus>,
    /// Shared screen layout, for the layout editor.
    pub layout: Option<LayoutUpdate>,
    /// The saved network profile this device is on, if any.
    pub network_profile: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: DeviceId,
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub rtt_us: Option<u64>,
    pub clock_offset_us: Option<i64>,
    /// Connected, Paused (greyed out in the layout) or Left.
    pub state: DeviceState,
}
