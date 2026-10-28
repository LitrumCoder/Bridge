//! Node assembly: wires transport, discovery, platform services and features together,
//! and serves the local IPC API.

mod bulk;
mod node;
mod pairing_uri;
pub mod profiles;
mod trust;

use std::net::SocketAddr;
use std::sync::Arc;

use bridge_core::PairingSecret;
use bridge_core::state::StateCommand;
use bridge_feature_files::{Direction, FilesHandle, TransferState, TransferStatus};
use bridge_ipc::{
    DeviceEntry, IpcHandler, MidiStreamEntry, NearbyEntry, PairTarget, ProfileEntry, Request,
    Response, TransferEntry, UiEvent,
};
use bridge_protocol::{DeviceId, PermissionId};
use bridge_transport::ExpectedPeer;
use tokio::sync::Notify;

pub use node::{LocalSender, NodeBuilder, NodeError, NodeEvent, NodeHandle, bind_transport};
pub use pairing_uri::{PairingInvite, QrPayload, parse_pairing_uri};
pub use trust::{ALL_PERMISSIONS, TrustStore, TrustedPeer, unix_now};

// TODO(M9): autostart registration.

/// Feature handles the IPC API reaches into.
#[derive(Clone, Default)]
pub struct Services {
    pub files: Option<FilesHandle>,
    pub midi: Option<bridge_feature_midi::MidiHandle>,
    pub events: UiEvents,
}

/// The last router events, numbered, for UIs that poll.
#[derive(Clone, Default)]
pub struct UiEvents(Arc<std::sync::Mutex<EventLog>>);

type EventLog = (u64, std::collections::VecDeque<(u64, UiEvent)>);

impl UiEvents {
    pub fn push(&self, e: UiEvent) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        g.0 += 1;
        let n = g.0;
        g.1.push_back((n, e));
        if g.1.len() > 64 {
            g.1.pop_front();
        }
    }

    pub fn since(&self, since: u64) -> (u64, Vec<UiEvent>) {
        let g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        (
            g.0,
            g.1.iter()
                .filter(|(n, _)| *n > since)
                .map(|(_, e)| e.clone())
                .collect(),
        )
    }
}

/// IPC request handler for a running node. `shutdown` is notified on `Request::Shutdown`.
pub fn ipc_handler(node: NodeHandle, shutdown: Arc<Notify>, services: Services) -> IpcHandler {
    Arc::new(move |req| {
        let node = node.clone();
        let shutdown = shutdown.clone();
        let services = services.clone();
        Box::pin(async move { handle_request(&node, &shutdown, &services, req).await })
    })
}

fn transfer_entry(s: TransferStatus) -> TransferEntry {
    let state = match s.state {
        TransferState::Running => "running".to_owned(),
        TransferState::Done => "done".to_owned(),
        TransferState::Cancelled => "cancelled".to_owned(),
        TransferState::Failed(why) => format!("failed: {why}"),
        TransferState::Declined(r) => format!("declined: {r:?}"),
    };
    TransferEntry {
        id: s.id.to_string(),
        device: s.peer,
        outgoing: s.direction == Direction::Outgoing,
        items: s.items,
        bytes: s.bytes,
        done_bytes: s.done_bytes,
        state,
        names: s.names,
    }
}

async fn files_request(node: &NodeHandle, files: &FilesHandle, req: Request) -> Response {
    match req {
        Request::Send { device, paths } => {
            let Some(id) = resolve(node, &device) else {
                return Response::Error(format!("no paired device matches {device}"));
            };
            match files
                .send(id, paths.into_iter().map(Into::into).collect())
                .await
            {
                Ok(t) => Response::Transfer { id: t.to_string() },
                Err(e) => Response::Error(e),
            }
        }
        Request::Transfers => {
            Response::Transfers(files.transfers().into_iter().map(transfer_entry).collect())
        }
        Request::CancelTransfer { id } => {
            let found = files
                .transfers()
                .into_iter()
                .find(|t| t.id.to_string().starts_with(&id.to_lowercase()));
            match found {
                Some(t) => {
                    files.cancel(t.id);
                    Response::Ok
                }
                None => Response::Error(format!("no transfer {id}")),
            }
        }
        _ => Response::Error("unexpected request".into()),
    }
}

async fn handle_request(
    node: &NodeHandle,
    shutdown: &Notify,
    services: &Services,
    req: Request,
) -> Response {
    match req {
        r @ (Request::Send { .. } | Request::Transfers | Request::CancelTransfer { .. }) => {
            match &services.files {
                Some(f) => files_request(node, f, r).await,
                None => Response::Error("file transfers are disabled on this device".into()),
            }
        }
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
        Request::SetLayout { screens } => match node.set_layout(screens) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e),
        },
        Request::MidiStreams => Response::Midi(
            services
                .midi
                .as_ref()
                .map(|m| {
                    m.streams()
                        .into_iter()
                        .map(|s| MidiStreamEntry {
                            device: s.peer,
                            port: s.port,
                            to_instrument: s.to_instrument,
                            events: s.stats.events,
                            late: s.stats.late,
                            min_transit_us: s.stats.min_transit_us,
                            buffer_us: s.stats.buffer_us,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        Request::Events { since } => {
            let (next, events) = services.events.since(since);
            Response::Events { next, events }
        }
        Request::Pause => state(node, StateCommand::Pause),
        Request::Resume => state(node, StateCommand::Resume),
        Request::Leave => state(node, StateCommand::Leave),
        Request::Rejoin => state(node, StateCommand::Rejoin),
        Request::Profiles => {
            let (profiles, current) = node.profiles().await;
            Response::Profiles(
                profiles
                    .into_iter()
                    .map(|p| ProfileEntry {
                        current: current.as_deref() == Some(p.name.as_str()),
                        networks: p.fingerprints.len() as u32,
                        auto_join: p.auto_join,
                        name: p.name,
                    })
                    .collect(),
            )
        }
        Request::SaveProfile { name, auto_join } => {
            match node.save_profile(name, auto_join).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            }
        }
        Request::ForgetProfile { name } => {
            if node.forget_profile(name.clone()).await {
                Response::Ok
            } else {
                Response::Error(format!("no profile {name}"))
            }
        }
        Request::Shutdown => {
            shutdown.notify_one();
            Response::Ok
        }
    }
}

fn state(node: &NodeHandle, cmd: StateCommand) -> Response {
    node.set_state(cmd);
    Response::Ok
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

/// Finds a paired device by full id, a unique fingerprint prefix or its name.
fn resolve(node: &NodeHandle, text: &str) -> Option<DeviceId> {
    let text = text.to_lowercase();
    let all = node.trust().all();
    let named: Vec<_> = all
        .iter()
        .filter(|p| p.name.as_deref().is_some_and(|n| n.to_lowercase() == text))
        .collect();
    if let [one] = named.as_slice() {
        return Some(one.id);
    }
    let matches: Vec<_> = all
        .iter()
        .filter(|p| p.id.to_base32().starts_with(&text))
        .collect();
    match matches.as_slice() {
        [one] => Some(one.id),
        _ => None,
    }
}
