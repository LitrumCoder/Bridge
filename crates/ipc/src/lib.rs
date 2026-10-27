//! Local IPC between `bridged` and its clients (ADR-0011): a named pipe on Windows, a
//! Unix socket elsewhere, frames of `u32 LE length ‖ postcard`. Only the current user
//! can connect.

mod status;
mod transport;

use std::net::SocketAddr;

use bridge_protocol::DeviceId;
use serde::{Deserialize, Serialize};

pub use status::{NodeStatus, PeerStatus};
pub use transport::{IpcClient, IpcError, IpcHandler, IpcServer, default_endpoint};

/// Bumped on any incompatible change of the types below.
pub const IPC_VERSION: u16 = 1;
pub const MAX_IPC_FRAME: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Status,
    /// Paired devices.
    Devices,
    /// Devices announced on the local network.
    Nearby,
    /// Keep a connection to a paired device at a fixed address (manual add by IP).
    Connect {
        addr: SocketAddr,
        device: DeviceId,
    },
    /// Show a code and QR link on this device for two minutes.
    OpenPairing,
    ClosePairing,
    /// Pair with another device whose window is open.
    Pair {
        target: PairTarget,
        code: String,
    },
    /// Forget a device (full id or unique fingerprint prefix).
    Unpair {
        device: String,
    },
    SetPermissions {
        device: String,
        granted: Vec<u16>,
    },
    Shutdown,
    /// Send files and folders to a device (paths on this machine).
    Send {
        device: String,
        paths: Vec<String>,
    },
    Transfers,
    CancelTransfer {
        id: String,
    },
    /// Device state (ARCHITECTURE §4.2).
    Pause,
    Resume,
    Leave,
    Rejoin,
    /// Saved network profiles (ADR-0016).
    Profiles,
    /// Remember the current network as a profile.
    SaveProfile {
        name: String,
        auto_join: bool,
    },
    ForgetProfile {
        name: String,
    },
    // TODO(M9): Subscribe for the desktop UI event stream.
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PairTarget {
    /// A nearby device by fingerprint prefix.
    Nearby(String),
    /// Manual add by IP when mDNS does not work on the network.
    Address(SocketAddr),
    /// A scanned or pasted `bridge://pair?...` link (carries its own secret).
    Uri(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: DeviceId,
    pub name: Option<String>,
    pub connected: bool,
    pub granted: Vec<u16>,
    pub last_seen: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NearbyEntry {
    pub id: DeviceId,
    pub name: Option<String>,
    pub addrs: Vec<SocketAddr>,
    pub paired: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Status(NodeStatus),
    Devices(Vec<DeviceEntry>),
    Nearby(Vec<NearbyEntry>),
    PairingInvite {
        code: String,
        qr_uri: String,
        expires_secs: u64,
    },
    Paired {
        device: DeviceId,
    },
    Error(String),
    Transfer {
        id: String,
    },
    Transfers(Vec<TransferEntry>),
    Profiles(Vec<ProfileEntry>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub name: String,
    pub networks: u32,
    pub auto_join: bool,
    /// This device is on this profile's network now.
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferEntry {
    pub id: String,
    pub device: DeviceId,
    pub outgoing: bool,
    pub items: u32,
    pub bytes: u64,
    pub done_bytes: u64,
    /// `running`, `done`, `cancelled`, `failed: …`, `declined: …`.
    pub state: String,
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Frame<T> {
    pub(crate) version: u16,
    pub(crate) id: u64,
    pub(crate) body: T,
}
