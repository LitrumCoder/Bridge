//! Node assembly: wires transport, discovery, platform services and features together,
//! and serves the local IPC API.

mod node;
mod pairing_uri;
mod trust;

use std::net::SocketAddr;
use std::sync::Arc;

use bridge_core::PairingSecret;
use bridge_ipc::{DeviceEntry, IpcHandler, NearbyEntry, PairTarget, Request, Response};
use bridge_protocol::{DeviceId, PermissionId};
use bridge_transport::ExpectedPeer;
use tokio::sync::Notify;

pub use node::{LocalSender, NodeBuilder, NodeError, NodeEvent, NodeHandle, bind_transport};
pub use pairing_uri::{PairingInvite, QrPayload, parse_pairing_uri};
pub use trust::{ALL_PERMISSIONS, TrustStore, TrustedPeer, unix_now};

// TODO(M9): autostart registration.

/// IPC request handler for a running node. `shutdown` is notified on `Request::Shutdown`.
pub fn ipc_handler(node: NodeHandle, shutdown: Arc<Notify>) -> IpcHandler {
    Arc::new(move |req| {
        let node = node.clone();
        let shutdown = shutdown.clone();
        Box::pin(async move { handle_request(&node, &shutdown, req).await })
    })
}

async fn handle_request(node: &NodeHandle, shutdown: &Notify, req: Request) -> Response {
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
                        name: live.map(|p| p.name.clone()).or(t.name),
                        connected: live.is_some(),
                        granted: t.granted.iter().map(|g| g.0).collect(),
                        last_seen: t.last_seen,
                    }
                })
                .collect();
            Response::Devices(devices)
        }
        Request::Nearby => {
            let trust = node.trust();
            Response::Nearby(
                node.nearby()
                    .into_iter()
                    .map(|d| NearbyEntry {
                        id: d.id,
                        name: d.name,
                        addrs: d.addrs,
                        paired: trust.get(&d.id).is_some(),
                    })
                    .collect(),
            )
        }
        Request::Connect { addr, device } => {
            if node.trust().get(&device).is_none() {
                return Response::Error(format!("device {device} is not paired; pair it first"));
            }
            node.dial(addr, device);
            Response::Ok
        }
        Request::OpenPairing => match node.open_pairing().await {
            Ok(i) => Response::PairingInvite {
                code: i.code,
                qr_uri: i.qr_uri,
                expires_secs: i.expires_in.as_secs(),
            },
            Err(e) => Response::Error(e.to_string()),
        },
        Request::ClosePairing => {
            node.close_pairing();
            Response::Ok
        }
        Request::Pair { target, code } => pair(node, target, &code).await,
        Request::Unpair { device } => {
            let Some(id) = resolve(node, &device) else {
                return Response::Error(format!("no paired device matches {device}"));
            };
            node.unpair(id);
            Response::Ok
        }
        Request::SetPermissions { device, granted } => {
            let Some(id) = resolve(node, &device) else {
                return Response::Error(format!("no paired device matches {device}"));
            };
            node.set_permissions(id, granted.into_iter().map(PermissionId).collect());
            Response::Ok
        }
        Request::Shutdown => {
            shutdown.notify_one();
            Response::Ok
        }
    }
}

async fn pair(node: &NodeHandle, target: PairTarget, code: &str) -> Response {
    let (addr, expect, secret): (SocketAddr, ExpectedPeer, PairingSecret) = match target {
        PairTarget::Uri(uri) => {
            let Some(p) = parse_pairing_uri(&uri) else {
                return Response::Error("this is not a Bridge pairing link".into());
            };
            let Some(addr) = p.addrs.first().copied() else {
                return Response::Error("the pairing link has no address".into());
            };
            (addr, ExpectedPeer::Device(p.device), p.secret)
        }
        PairTarget::Address(addr) => {
            let Some(secret) = PairingSecret::from_code(code) else {
                return Response::Error("the code must be 6 digits".into());
            };
            (addr, ExpectedPeer::Any, secret)
        }
        PairTarget::Nearby(prefix) => {
            let Some(secret) = PairingSecret::from_code(code) else {
                return Response::Error("the code must be 6 digits".into());
            };
            let matches: Vec<_> = node
                .nearby()
                .into_iter()
                .filter(|d| d.id.to_base32().starts_with(&prefix.to_lowercase()))
                .collect();
            let [found] = matches.as_slice() else {
                return Response::Error(format!(
                    "{} nearby devices match {prefix}; use more characters or an address",
                    matches.len()
                ));
            };
            let Some(addr) = found
                .addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(found.addrs.first())
                .copied()
            else {
                return Response::Error("the device announced no address".into());
            };
            (addr, ExpectedPeer::Device(found.id), secret)
        }
    };
    match node.pair(addr, expect, secret).await {
        Ok(id) => Response::Paired { device: id },
        Err(e) => Response::Error(e.to_string()),
    }
}

/// Finds a paired device by full id or a unique fingerprint prefix.
fn resolve(node: &NodeHandle, text: &str) -> Option<DeviceId> {
    let text = text.to_lowercase();
    let all = node.trust().all();
    let matches: Vec<_> = all
        .iter()
        .filter(|p| p.id.to_base32().starts_with(&text))
        .collect();
    match matches.as_slice() {
        [one] => Some(one.id),
        _ => None,
    }
}
