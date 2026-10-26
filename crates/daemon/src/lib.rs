//! Node assembly: wires transport, discovery, platform services and features together,
//! and serves the local IPC API.

mod node;

use std::sync::Arc;

use bridge_ipc::{DeviceEntry, IpcHandler, Request, Response};
use tokio::sync::Notify;

pub use node::{
    NodeBuilder, NodeError, NodeEvent, NodeHandle, TrustStore, TrustedPeer, bind_transport,
};

// TODO(M3): discovery (mDNS) and pairing; TODO(M9): autostart registration.

/// IPC request handler for a running node. `shutdown` is notified on `Request::Shutdown`.
pub fn ipc_handler(node: NodeHandle, shutdown: Arc<Notify>) -> IpcHandler {
    Arc::new(move |req| {
        let node = node.clone();
        let shutdown = shutdown.clone();
        Box::pin(async move { handle_request(&node, &shutdown, req) })
    })
}

fn handle_request(node: &NodeHandle, shutdown: &Notify, req: Request) -> Response {
    match req {
        Request::Status => Response::Status(node.status()),
        Request::Devices => {
            let status = node.status();
            let devices = node
                .trust()
                .all()
                .into_iter()
                .map(|t| {
                    let live = status.peers.iter().find(|p| p.id == t.id);
                    DeviceEntry {
                        id: t.id,
                        name: live.map(|p| p.name.clone()),
                        connected: live.is_some(),
                    }
                })
                .collect();
            Response::Devices(devices)
        }
        Request::Connect { addr, device } => {
            if node.trust().get(&device).is_none() {
                return Response::Error(format!("device {device} is not trusted; pair it first"));
            }
            node.dial(addr, device);
            Response::Ok
        }
        Request::Shutdown => {
            shutdown.notify_one();
            Response::Ok
        }
    }
}
