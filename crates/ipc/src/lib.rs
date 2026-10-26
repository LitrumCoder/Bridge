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
    Devices,
    /// Keep a connection to a known device at a fixed address (manual add by IP).
    Connect {
        addr: SocketAddr,
        device: DeviceId,
    },
    Shutdown,
    // TODO(M3): Pair, Unpair; TODO(M6a): Send; TODO(M8): Pause, Leave, Rejoin;
    // TODO(M9): Subscribe for the desktop UI event stream.
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: DeviceId,
    pub name: Option<String>,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Status(NodeStatus),
    Devices(Vec<DeviceEntry>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Frame<T> {
    pub(crate) version: u16,
    pub(crate) id: u64,
    pub(crate) body: T,
}
