//! Integration: full nodes (driver + core + QUIC) talking over the simulated network.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::{Feature, FeatureCx, FeatureError, FeatureInfo, PeerDownReason, PeerId, decode};
use bridge_daemon::NodeEvent;
use bridge_protocol::{ChannelClass, FeatureId, PermissionId, ProtocolVersion};
use bridge_testkit::{LinkConfig, Network, mutually_trusting};
use serde::{Deserialize, Serialize};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct Note(String);

/// Sends a greeting on the clipboard channel and a datagram when a peer comes up.
struct Greeter {
    me: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Feature for Greeter {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: FeatureId(250),
            name: "greeter",
            versions: 1..=1,
            channel: ChannelClass::Clipboard,
            permission: Some(PermissionId::CLIPBOARD),
        }
    }

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, _version: u16) {
        cx.send(peer, 1, &Note(format!("hello from {}", self.me)))
            .unwrap();
        cx.send_datagram(peer, self.me.as_bytes().to_vec());
    }

    fn on_peer_down(&mut self, _cx: &mut FeatureCx<'_>, _peer: PeerId, reason: PeerDownReason) {
        self.seen.lock().unwrap().push(format!("down {reason:?}"));
    }

    fn on_message(
        &mut self,
        _cx: &mut FeatureCx<'_>,
        _peer: PeerId,
        _kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        let Note(text) = decode(payload)?;
        self.seen.lock().unwrap().push(text);
        Ok(())
    }

    fn on_datagram(
        &mut self,
        _cx: &mut FeatureCx<'_>,
        _peer: PeerId,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("dgram {}", String::from_utf8_lossy(payload)));
        Ok(())
    }
}

impl bridge_protocol::Validate for Note {}

async fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_form_a_mesh_and_features_talk() {
    let net = Network::new(11);
    net.set_default_link(LinkConfig {
        latency: Duration::from_millis(1),
        ..LinkConfig::default()
    });
    let specs = mutually_trusting(&net, 3).unwrap();
    let logs: Vec<Arc<Mutex<Vec<String>>>> = (0..3).map(|_| Arc::default()).collect();
    let nodes: Vec<_> = specs
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let seen = logs[i].clone();
            spec.start(&format!("node{i}"), |b| {
                b.feature(Greeter {
                    me: format!("node{i}"),
                    seen,
                })
                .unwrap()
            })
        })
        .collect();

    nodes[0].handle.dial(nodes[1].addr, nodes[1].id);
    nodes[0].handle.dial(nodes[2].addr, nodes[2].id);
    nodes[2].handle.dial(nodes[1].addr, nodes[1].id);

    for (i, n) in nodes.iter().enumerate() {
        for (j, m) in nodes.iter().enumerate() {
            if i != j {
                assert!(
                    n.handle.wait_connected(m.id, true, WAIT).await,
                    "node{i} → node{j}"
                );
            }
        }
    }
    assert!(wait_until(|| logs.iter().all(|l| l.lock().unwrap().len() >= 4)).await);
    let log1 = logs[1].lock().unwrap().clone();
    assert!(log1.contains(&"hello from node0".to_string()), "{log1:?}");
    assert!(log1.contains(&"hello from node2".to_string()), "{log1:?}");
    assert!(log1.contains(&"dgram node0".to_string()), "{log1:?}");

    assert!(
        wait_until(|| nodes[0]
            .handle
            .status()
            .peers
            .iter()
            .all(|p| p.rtt_us.is_some()))
        .await
    );
    let status = nodes[0].handle.status();
    assert_eq!(status.peers.len(), 2);
    assert!(status.peers.iter().any(|p| p.name == "node1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_dials_leave_one_connection() {
    let net = Network::new(5);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    a.handle.dial(b.addr, b.id);
    b.handle.dial(a.addr, a.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.handle.status().peers.len(), 1);
    assert_eq!(b.handle.status().peers.len(), 1);
    assert!(a.handle.is_connected(&b.id) && b.handle.is_connected(&a.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_device_is_refused_and_dialler_gives_up() {
    let net = Network::new(9);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap();
    let b = specs.next().unwrap();
    // B forgets A before starting.
    b.trust.remove(&a.identity.id());
    let a = a.start("a", |x| x);
    let b = b.start("b", |x| x);
    let mut events = a.handle.subscribe();
    a.handle.dial(b.addr, b.id);
    let stopped = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(NodeEvent::DialStopped { reason, .. }) = events.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("dial must stop");
    assert!(stopped.contains("does not trust"), "{stopped}");
    assert!(!b.handle.is_connected(&a.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_protocol_version_is_refused() {
    let net = Network::new(13);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| {
        b.protocol_version(ProtocolVersion { major: 2, minor: 0 })
    });
    let b = specs.next().unwrap().start("b", |b| b);
    let mut events = a.handle.subscribe();
    a.handle.dial(b.addr, b.id);
    let reason = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(NodeEvent::DialStopped { reason, .. }) = events.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("dial must stop");
    assert!(reason.contains("incompatible"), "{reason}");
    assert!(!a.handle.is_connected(&b.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partition_drops_the_peer_and_healing_reconnects() {
    let net = Network::new(21);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let seen_a: Arc<Mutex<Vec<String>>> = Arc::default();
    let sa = seen_a.clone();
    let a = specs.next().unwrap().start("a", |b| {
        b.feature(Greeter {
            me: "a".into(),
            seen: sa,
        })
        .unwrap()
    });
    let seen_b: Arc<Mutex<Vec<String>>> = Arc::default();
    let sb = seen_b.clone();
    let b = specs.next().unwrap().start("b", |x| {
        x.feature(Greeter {
            me: "b".into(),
            seen: sb,
        })
        .unwrap()
    });
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(wait_until(|| seen_a.lock().unwrap().iter().any(|e| e == "hello from b")).await);

    net.partition(a.addr, b.addr);
    assert!(
        a.handle.wait_connected(b.id, false, WAIT).await,
        "idle timeout must notice the partition"
    );
    assert!(b.handle.wait_connected(a.id, false, WAIT).await);
    assert!(wait_until(|| seen_a.lock().unwrap().iter().any(|e| e == "down Lost")).await);

    net.heal(a.addr, b.addr);
    assert!(
        a.handle.wait_connected(b.id, true, WAIT).await,
        "dialler must reconnect after healing"
    );
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lossy_jittery_link_still_connects() {
    let net = Network::new(33);
    net.set_default_link(LinkConfig {
        latency: Duration::from_millis(15),
        jitter: Duration::from_millis(10),
        loss: 0.1,
        ..LinkConfig::default()
    });
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(
        wait_until(|| a
            .handle
            .status()
            .peers
            .first()
            .and_then(|p| p.rtt_us)
            .is_some())
        .await
    );
    let rtt = a.handle.status().peers[0].rtt_us.unwrap();
    assert!(
        rtt >= 30_000,
        "rtt reflects the simulated latency: {rtt} µs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_closes_and_stops_redialling() {
    let net = Network::new(41);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    let mut events = a.handle.subscribe();
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    b.handle.unpair(a.id);
    let reason = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(NodeEvent::DialStopped { reason, .. }) = events.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("dial must stop after revocation");
    assert!(reason.contains("revoked"), "{reason}");
}
