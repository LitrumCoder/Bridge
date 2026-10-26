//! Integration: pairing, discovery and trust over real QUIC on the simulated network.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use bridge_core::PairingSecret;
use bridge_daemon::{NodeEvent, parse_pairing_uri};
use bridge_discovery::MemoryBus;
use bridge_protocol::PermissionId;
use bridge_testkit::{Network, mutually_trusting, strangers};
use bridge_transport::{ExpectedPeer, TrustPolicy};

const WAIT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_by_code_trusts_both_sides_and_connects() {
    let net = Network::new(101);
    let mut specs = strangers(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("laptop", |b| b);
    let b = specs.next().unwrap().start("desktop", |b| b);

    let invite = b.handle.open_pairing().await.unwrap();
    assert_eq!(invite.code.len(), 6);
    assert!(
        b.handle.trust().pairing_open(),
        "TLS lets unknown devices in while the window is open"
    );

    let secret = PairingSecret::from_code(&invite.code).unwrap();
    let paired = a
        .handle
        .pair(b.addr, ExpectedPeer::Any, secret)
        .await
        .unwrap();
    assert_eq!(paired, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);

    let on_a = a.handle.trust().get(&b.id).expect("A trusts B");
    let on_b = b.handle.trust().get(&a.id).expect("B trusts A");
    assert_eq!(
        on_b.granted.len(),
        4,
        "own devices get every permission by default"
    );
    assert!(on_a.granted.contains(&PermissionId::INPUT));
    assert!(
        !b.handle.trust().pairing_open(),
        "the window closes after one pairing"
    );
    assert!(
        tokio::time::timeout(WAIT, async {
            while a.handle.trust().get(&b.id).and_then(|p| p.name).as_deref() != Some("desktop") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_by_qr_link_pins_the_device() {
    let net = Network::new(102);
    let mut specs = strangers(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("phone", |b| b);
    let b = specs.next().unwrap().start("desktop", |b| b);
    let invite = b.handle.open_pairing().await.unwrap();
    let qr = parse_pairing_uri(&invite.qr_uri).unwrap();
    assert_eq!(qr.device, b.id);
    // The URI carries the simulated listen address only if it is not unspecified;
    // in the simulator we dial the socket address directly.
    let paired = a
        .handle
        .pair(b.addr, ExpectedPeer::Device(qr.device), qr.secret)
        .await
        .unwrap();
    assert_eq!(paired, b.id);
    assert!(b.handle.wait_connected(a.id, true, WAIT).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_code_is_refused_and_burns_the_window() {
    let net = Network::new(103);
    let mut specs = strangers(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    let invite = b.handle.open_pairing().await.unwrap();
    let wrong = if invite.code == "000000" {
        "000001"
    } else {
        "000000"
    };
    let err = a
        .handle
        .pair(
            b.addr,
            ExpectedPeer::Any,
            PairingSecret::from_code(wrong).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("code"), "{err}");
    assert!(a.handle.trust().get(&b.id).is_none());
    assert!(b.handle.trust().get(&a.id).is_none());
    // The right code no longer works: one attempt per code.
    let again = a
        .handle
        .pair(
            b.addr,
            ExpectedPeer::Any,
            PairingSecret::from_code(&invite.code).unwrap(),
        )
        .await;
    assert!(again.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_device_cannot_connect_without_an_open_window() {
    let net = Network::new(104);
    let mut specs = strangers(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    let err = a
        .handle
        .pair(
            b.addr,
            ExpectedPeer::Any,
            PairingSecret::from_code("123456").unwrap(),
        )
        .await
        .unwrap_err();
    assert!(!err.to_string().is_empty());
    assert!(!b.handle.is_connected(&a.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_finds_trusted_devices_and_connects_automatically() {
    let net = Network::new(105);
    let bus = MemoryBus::new();
    let mut specs = mutually_trusting(&net, 3).unwrap().into_iter();
    let s0 = specs.next().unwrap();
    let s1 = specs.next().unwrap();
    let stranger = bridge_testkit::strangers(&net, 1).unwrap().pop().unwrap();
    let (id0, addr0) = (s0.identity.id(), s0.socket.addr());
    let (id1, addr1) = (s1.identity.id(), s1.socket.addr());
    let (ids, addrs) = (stranger.identity.id(), stranger.socket.addr());
    let n0 = s0.start("n0", |b| b.discovery(bus.endpoint(id0, addr0)));
    let n1 = s1.start("n1", |b| b.discovery(bus.endpoint(id1, addr1)));
    let ns = stranger.start("stranger", |b| b.discovery(bus.endpoint(ids, addrs)));

    assert!(
        n0.handle.wait_connected(id1, true, WAIT).await,
        "trusted devices connect by themselves"
    );
    assert!(n1.handle.wait_connected(id0, true, WAIT).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!n0.handle.is_connected(&ids) && !ns.handle.is_connected(&id0));
    let nearby = n0.handle.nearby();
    assert!(
        nearby
            .iter()
            .any(|d| d.id == ids && d.name.as_deref() == Some("stranger"))
    );
    assert_eq!(nearby.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpairing_removes_trust_on_both_sides() {
    let net = Network::new(106);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    let mut events = a.handle.subscribe();
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(b.handle.unpair(a.id));
    assert!(b.handle.trust().get(&a.id).is_none());
    let got_unpaired = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(NodeEvent::Core(bridge_core::CoreEvent::Unpaired { .. })) =
                events.recv().await
            {
                return;
            }
        }
    })
    .await;
    assert!(got_unpaired.is_ok(), "the peer learns about the revocation");
    assert!(
        tokio::time::timeout(WAIT, async {
            while a.handle.trust().get(&b.id).is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_changes_reach_the_live_peer() {
    let net = Network::new(107);
    let mut specs = mutually_trusting(&net, 2).unwrap().into_iter();
    let a = specs.next().unwrap().start("a", |b| b);
    let b = specs.next().unwrap().start("b", |b| b);
    let mut events = a.handle.subscribe();
    a.handle.dial(b.addr, b.id);
    assert!(a.handle.wait_connected(b.id, true, WAIT).await);
    assert!(
        b.handle
            .set_permissions(a.id, vec![PermissionId::CLIPBOARD])
    );
    let got = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(NodeEvent::Core(bridge_core::CoreEvent::PeerPermissions { granted, .. })) =
                events.recv().await
                && granted == vec![PermissionId::CLIPBOARD]
            {
                return;
            }
        }
    })
    .await;
    assert!(got.is_ok());
    assert_eq!(
        b.handle.trust().get(&a.id).unwrap().granted,
        vec![PermissionId::CLIPBOARD]
    );
}
