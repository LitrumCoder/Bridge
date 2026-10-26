//! Two `Core`s wired back to back without any transport.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::*;
use bridge_protocol::control::{DeviceState, StateReason};
use bridge_protocol::{
    ChannelClass, CloseCode, DeviceId, FeatureId, PROTOCOL_VERSION, PermissionId, Platform,
    ProtocolVersion, SessionToken,
};
use serde::{Deserialize, Serialize};

const ECHO: FeatureId = FeatureId(200);

#[derive(Serialize, Deserialize)]
struct Note(u32);

impl bridge_protocol::Validate for Note {}

#[derive(Default)]
struct Log {
    events: Vec<String>,
}

struct Echo {
    log: Arc<Mutex<Log>>,
    permission: Option<PermissionId>,
}

impl Feature for Echo {
    fn info(&self) -> FeatureInfo {
        FeatureInfo {
            id: ECHO,
            name: "echo",
            versions: 1..=2,
            channel: ChannelClass::Clipboard,
            permission: self.permission,
        }
    }

    fn on_peer_up(&mut self, cx: &mut FeatureCx<'_>, peer: PeerId, version: u16) {
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("up {} v{version}", peer.fingerprint()));
        assert!(cx.peer(&peer).is_some());
    }

    fn on_peer_down(&mut self, _cx: &mut FeatureCx<'_>, peer: PeerId, reason: PeerDownReason) {
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("down {} {reason:?}", peer.fingerprint()));
    }

    fn on_message(
        &mut self,
        cx: &mut FeatureCx<'_>,
        peer: PeerId,
        kind: u16,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        let Note(n) = decode(payload)?;
        if n == 666 {
            return Err(FeatureError::Violation("evil number".into()));
        }
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("msg {kind} {n}"));
        if kind == 1 {
            cx.send(peer, 2, &Note(n + 1)).unwrap();
        }
        Ok(())
    }

    fn on_datagram(
        &mut self,
        _cx: &mut FeatureCx<'_>,
        _peer: PeerId,
        payload: &[u8],
    ) -> Result<(), FeatureError> {
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("dgram {payload:?}"));
        Ok(())
    }
}

struct Node {
    core: Core,
    log: Arc<Mutex<Log>>,
    id: DeviceId,
}

fn node(byte: u8, protocol: ProtocolVersion, permission: Option<PermissionId>) -> Node {
    let id = DeviceId([byte; 32]);
    let log = Arc::new(Mutex::new(Log::default()));
    let mut registry = FeatureRegistry::new();
    registry
        .register(Box::new(Echo {
            log: log.clone(),
            permission,
        }))
        .unwrap();
    let config = CoreConfig {
        device_id: id,
        name: format!("node-{byte}"),
        platform: Platform::LINUX,
        app_version: "0.1.0".into(),
        protocol,
        default_grants: vec![PermissionId::CLIPBOARD],
    };
    Node {
        core: Core::new(
            config,
            registry,
            Box::new(TestEntropy(u64::from(byte) * 7919)),
        ),
        log,
        id,
    }
}

/// Deterministic randomness for tests (never use outside tests).
struct TestEntropy(u64);

impl Entropy for TestEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (self.0 >> 56) as u8;
        }
    }
}

/// A bidirectional link between two cores: `a` sees conn `ca`, `b` sees conn `cb`.
struct Link {
    ca: ConnId,
    cb: ConnId,
}

#[derive(Default)]
struct Outcome {
    events_a: Vec<CoreEvent>,
    events_b: Vec<CoreEvent>,
    closed: Vec<(char, CloseCode, String)>,
}

/// Both ends of a link use ids `n` and `n + 1000`.
fn far_end(conn: ConnId) -> ConnId {
    if conn.0 >= 1000 {
        ConnId(conn.0 - 1000)
    } else {
        ConnId(conn.0 + 1000)
    }
}

/// Moves actions between the two cores until both are quiet.
fn pump(a: &mut Node, b: &mut Node, _link: &Link, now: Micros) -> Outcome {
    let mut out = Outcome::default();
    loop {
        let mut moved = false;
        while let Some(act) = a.core.poll_action() {
            moved = true;
            deliver(act, 'a', a, b, now, &mut out.events_a, &mut out.closed);
        }
        while let Some(act) = b.core.poll_action() {
            moved = true;
            deliver(act, 'b', b, a, now, &mut out.events_b, &mut out.closed);
        }
        if !moved {
            return out;
        }
    }
}

fn deliver(
    act: Action,
    side: char,
    me: &mut Node,
    other: &mut Node,
    now: Micros,
    events: &mut Vec<CoreEvent>,
    closed: &mut Vec<(char, CloseCode, String)>,
) {
    match act {
        Action::Send {
            conn,
            channel,
            envelope,
        } => other
            .core
            .handle_frame(now, far_end(conn), channel, &envelope),
        Action::SendDatagram { conn, bytes } => {
            other.core.handle_datagram(now, far_end(conn), &bytes)
        }
        Action::Close { conn, code, reason } => {
            closed.push((side, code, reason));
            me.core.handle_closed(now, conn, Some(code));
            other.core.handle_closed(now, far_end(conn), Some(code));
        }
        Action::Event(e) => events.push(e),
    }
}

fn info(
    conn: u64,
    role: Role,
    peer: DeviceId,
    trusted: bool,
    granted: &[PermissionId],
) -> ConnectInfo {
    ConnectInfo {
        conn: ConnId(conn),
        role,
        peer,
        trusted,
        granted: granted.to_vec(),
        pair: None,
        session: SessionToken([conn as u8; 16]),
    }
}

fn connect(a: &mut Node, b: &mut Node, conn: u64, trusted: bool, now: Micros) -> (Link, Outcome) {
    let link = Link {
        ca: ConnId(conn),
        cb: ConnId(conn + 1000),
    };
    let all = [PermissionId::CLIPBOARD];
    a.core
        .handle_connected(now, info(conn, Role::Initiator, b.id, true, &all));
    b.core
        .handle_connected(now, info(conn + 1000, Role::Responder, a.id, trusted, &all));
    let out = pump(a, b, &link, now);
    (link, out)
}

fn has_peer_up(events: &[CoreEvent], id: DeviceId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, CoreEvent::PeerUp(p) if p.id == id))
}

#[test]
fn handshake_activates_both_sides_and_negotiates_features() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (_, out) = connect(&mut a, &mut b, 1, true, Micros(1));
    assert!(has_peer_up(&out.events_a, b.id));
    assert!(has_peer_up(&out.events_b, a.id));
    let peers = a.core.peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].name, "node-2");
    assert_eq!(peers[0].features[0].id, ECHO);
    assert_eq!(peers[0].features[0].version, 2);
    assert!(
        a.log
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e.starts_with("up"))
    );
    assert!(out.closed.is_empty());
}

#[test]
fn feature_messages_and_datagrams_reach_the_peer_feature() {
    let mut a = node(1, PROTOCOL_VERSION, Some(PermissionId::CLIPBOARD));
    let mut b = node(2, PROTOCOL_VERSION, Some(PermissionId::CLIPBOARD));
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1));

    let env = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 1,
        payload: bridge_protocol::encode_payload(&Note(41)).unwrap(),
    };
    b.core
        .handle_frame(Micros(2), link.cb, ChannelClass::Clipboard, &env.to_bytes());
    pump(&mut a, &mut b, &link, Micros(2));
    assert!(
        b.log
            .lock()
            .unwrap()
            .events
            .contains(&"msg 1 41".to_string())
    );
    assert!(
        a.log
            .lock()
            .unwrap()
            .events
            .contains(&"msg 2 42".to_string())
    );

    let dgram = bridge_protocol::encode_datagram(ECHO, &[7, 8]);
    a.core.handle_datagram(Micros(3), link.ca, &dgram);
    assert!(
        a.log
            .lock()
            .unwrap()
            .events
            .contains(&"dgram [7, 8]".to_string())
    );
}

#[test]
fn messages_without_our_permission_are_dropped() {
    let mut a = node(1, PROTOCOL_VERSION, Some(PermissionId::FILES));
    let mut b = node(2, PROTOCOL_VERSION, Some(PermissionId::FILES));
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1));
    let env = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 3,
        payload: bridge_protocol::encode_payload(&Note(1)).unwrap(),
    };
    a.core
        .handle_frame(Micros(2), link.ca, ChannelClass::Clipboard, &env.to_bytes());
    assert!(
        !a.log
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e.starts_with("msg"))
    );
    assert_eq!(a.core.stats().not_permitted, 1);
}

#[test]
fn incompatible_major_version_is_rejected_with_reason() {
    let mut a = node(1, ProtocolVersion { major: 2, minor: 0 }, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (_, out) = connect(&mut a, &mut b, 1, true, Micros(1));
    let rejected = out.events_a.iter().find_map(|e| match e {
        CoreEvent::Rejected {
            code,
            protocol,
            message,
            ..
        } => Some((*code, *protocol, message.clone())),
        _ => None,
    });
    let (code, protocol, message) = rejected.expect("initiator learns why");
    assert_eq!(code, CloseCode::INCOMPATIBLE_VERSION);
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert!(message.contains("2.0"));
    assert!(a.core.peers().is_empty() && b.core.peers().is_empty());
}

#[test]
fn untrusted_device_is_refused() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (_, out) = connect(&mut a, &mut b, 1, false, Micros(1));
    assert!(out.events_a.iter().any(|e| matches!(
        e,
        CoreEvent::Rejected {
            code: CloseCode::NOT_PAIRED,
            ..
        }
    )));
    assert!(b.core.peers().is_empty());
}

/// A dials B with a pairing secret; B is untrusted-by-default for A.
fn pair_attempt(
    a: &mut Node,
    b: &mut Node,
    conn: u64,
    secret: PairingSecret,
    now: Micros,
) -> Outcome {
    let link = Link {
        ca: ConnId(conn),
        cb: ConnId(conn + 1000),
    };
    let mut ia = info(conn, Role::Initiator, b.id, false, &[]);
    ia.pair = Some(secret);
    a.core.handle_connected(now, ia);
    b.core
        .handle_connected(now, info(conn + 1000, Role::Responder, a.id, false, &[]));
    pump(a, b, &link, now)
}

fn rejected_with(events: &[CoreEvent], code: CloseCode) -> Option<String> {
    events.iter().find_map(|e| match e {
        CoreEvent::Rejected {
            code: c, message, ..
        } if *c == code => Some(message.clone()),
        _ => None,
    })
}

#[test]
fn pairing_is_refused_while_the_window_is_closed() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let out = pair_attempt(
        &mut a,
        &mut b,
        1,
        PairingSecret::from_code("123456").unwrap(),
        Micros(1),
    );
    let msg = rejected_with(&out.events_a, CloseCode::PAIRING_FAILED).expect("refused");
    assert!(msg.contains("not open"), "{msg}");
}

#[test]
fn pairing_with_the_right_code_pairs_and_activates() {
    let mut a = node(1, PROTOCOL_VERSION, Some(PermissionId::CLIPBOARD));
    let mut b = node(2, PROTOCOL_VERSION, Some(PermissionId::CLIPBOARD));
    let (code, _qr) = b.core.open_pairing(Micros(1), PAIRING_WINDOW).unwrap();
    assert!(b.core.pairing_open(Micros(1)));
    let out = pair_attempt(
        &mut a,
        &mut b,
        1,
        PairingSecret::from_code(code.code().unwrap()).unwrap(),
        Micros(2),
    );
    assert!(out.events_a.iter().any(|e| matches!(
        e,
        CoreEvent::Paired {
            role: Role::Initiator,
            ..
        }
    )));
    assert!(out.events_b.iter().any(|e| matches!(
        e,
        CoreEvent::Paired {
            role: Role::Responder,
            ..
        }
    )));
    assert!(has_peer_up(&out.events_a, b.id) && has_peer_up(&out.events_b, a.id));
    assert!(!b.core.pairing_open(Micros(3)), "one pairing per window");
    // The responder granted the default permissions to the new device.
    assert!(out.events_a.iter().any(|e| matches!(
        e,
        CoreEvent::PeerPermissions { granted, .. } if granted == &vec![PermissionId::CLIPBOARD]
    )));
}

#[test]
fn pairing_with_the_qr_secret_works() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (_code, qr) = b.core.open_pairing(Micros(1), PAIRING_WINDOW).unwrap();
    let out = pair_attempt(&mut a, &mut b, 1, qr, Micros(2));
    assert!(has_peer_up(&out.events_a, b.id));
}

#[test]
fn wrong_code_fails_closes_the_window_and_locks_after_three() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    for attempt in 0..3u64 {
        let now = Micros(1_000_000 + attempt * 1_000_000);
        let (code, _) = b.core.open_pairing(now, PAIRING_WINDOW).unwrap();
        let wrong = if code.code() == Some("000000") {
            "000001"
        } else {
            "000000"
        };
        let out = pair_attempt(
            &mut a,
            &mut b,
            10 + attempt,
            PairingSecret::from_code(wrong).unwrap(),
            now,
        );
        assert!(rejected_with(&out.events_a, CloseCode::PAIRING_FAILED).is_some());
        assert!(
            out.events_b
                .iter()
                .any(|e| matches!(e, CoreEvent::PairingFailed { .. }))
        );
        assert!(!b.core.pairing_open(now), "a failed attempt burns the code");
        assert!(a.core.peers().is_empty());
    }
    let later = Micros(10_000_000);
    assert!(matches!(
        b.core.open_pairing(later, PAIRING_WINDOW),
        Err(PairingError::Locked { .. })
    ));
    let out = pair_attempt(
        &mut a,
        &mut b,
        50,
        PairingSecret::from_code("111111").unwrap(),
        later,
    );
    assert!(
        rejected_with(&out.events_a, CloseCode::PAIRING_FAILED)
            .unwrap()
            .contains("locked")
    );
}

#[test]
fn feature_traffic_during_pairing_from_the_initiator_is_a_violation_on_the_responder() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    b.core.open_pairing(Micros(1), PAIRING_WINDOW).unwrap();
    let mut ia = info(1, Role::Initiator, b.id, false, &[]);
    ia.pair = Some(PairingSecret::from_code("123456").unwrap());
    a.core.handle_connected(Micros(1), ia);
    b.core
        .handle_connected(Micros(1), info(1001, Role::Responder, a.id, false, &[]));
    let hello: Vec<Action> = std::iter::from_fn(|| a.core.poll_action()).collect();
    for act in hello {
        if let Action::Send {
            channel, envelope, ..
        } = act
        {
            b.core
                .handle_frame(Micros(1), ConnId(1001), channel, &envelope);
        }
    }
    let env = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 1,
        payload: vec![0],
    };
    b.core.handle_frame(
        Micros(1),
        ConnId(1001),
        ChannelClass::Clipboard,
        &env.to_bytes(),
    );
    let acts: Vec<Action> = std::iter::from_fn(|| b.core.poll_action()).collect();
    assert!(acts.iter().any(|x| matches!(
        x,
        Action::Close {
            code: CloseCode::PROTOCOL_VIOLATION,
            ..
        }
    )));
}

#[test]
fn hello_with_foreign_identity_is_a_violation() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let link = Link {
        ca: ConnId(1),
        cb: ConnId(1001),
    };
    a.core
        .handle_connected(Micros(1), info(1, Role::Initiator, b.id, true, &[]));
    // B's TLS layer claims the peer is device 9, but the hello says device 1.
    b.core.handle_connected(
        Micros(1),
        info(1001, Role::Responder, DeviceId([9; 32]), true, &[]),
    );
    let out = pump(&mut a, &mut b, &link, Micros(1));
    assert!(
        out.closed
            .iter()
            .any(|(s, c, _)| *s == 'b' && *c == CloseCode::PROTOCOL_VIOLATION)
    );
}

#[test]
fn feature_message_before_handshake_is_a_violation() {
    let mut b = node(2, PROTOCOL_VERSION, None);
    b.core.handle_connected(
        Micros(1),
        info(2, Role::Responder, DeviceId([1; 32]), true, &[]),
    );
    let env = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 1,
        payload: vec![1],
    };
    b.core.handle_frame(
        Micros(1),
        ConnId(2),
        ChannelClass::Clipboard,
        &env.to_bytes(),
    );
    let act = std::iter::from_fn(|| b.core.poll_action()).collect::<Vec<_>>();
    assert!(act.iter().any(|a| matches!(
        a,
        Action::Close {
            code: CloseCode::PROTOCOL_VIOLATION,
            ..
        }
    )));
}

#[test]
fn initiator_buffers_feature_traffic_that_overtakes_server_hello() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    a.core
        .handle_connected(Micros(1), info(1, Role::Initiator, b.id, true, &[]));
    b.core
        .handle_connected(Micros(1), info(1001, Role::Responder, a.id, true, &[]));
    let hello = std::iter::from_fn(|| a.core.poll_action()).collect::<Vec<_>>();
    for act in hello {
        if let Action::Send {
            channel, envelope, ..
        } = act
        {
            b.core
                .handle_frame(Micros(1), ConnId(1001), channel, &envelope);
        }
    }
    // B is active now. Deliver its feature traffic to A before its control frames.
    let note = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 3,
        payload: bridge_protocol::encode_payload(&Note(5)).unwrap(),
    };
    a.core.handle_frame(
        Micros(2),
        ConnId(1),
        ChannelClass::Clipboard,
        &note.to_bytes(),
    );
    a.core.handle_datagram(
        Micros(2),
        ConnId(1),
        &bridge_protocol::encode_datagram(ECHO, &[1]),
    );
    assert!(a.log.lock().unwrap().events.is_empty());
    let control: Vec<Action> = std::iter::from_fn(|| b.core.poll_action()).collect();
    for act in control {
        if let Action::Send {
            channel, envelope, ..
        } = act
        {
            a.core
                .handle_frame(Micros(3), ConnId(1), channel, &envelope);
        }
    }
    let log = a.log.lock().unwrap().events.clone();
    assert!(log.contains(&"msg 3 5".to_string()), "{log:?}");
    assert!(log.contains(&"dgram [1]".to_string()), "{log:?}");
    assert_eq!(a.core.stats().violations, 0);
}

#[test]
fn unbounded_early_traffic_is_a_violation() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    a.core.handle_connected(
        Micros(1),
        info(1, Role::Initiator, DeviceId([2; 32]), true, &[]),
    );
    let big = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 3,
        payload: vec![0; 64 * 1024],
    };
    for _ in 0..5 {
        a.core.handle_frame(
            Micros(2),
            ConnId(1),
            ChannelClass::Clipboard,
            &big.to_bytes(),
        );
    }
    let acts: Vec<Action> = std::iter::from_fn(|| a.core.poll_action()).collect();
    assert!(acts.iter().any(|x| matches!(
        x,
        Action::Close {
            code: CloseCode::PROTOCOL_VIOLATION,
            ..
        }
    )));
}

#[test]
fn feature_violation_closes_the_connection() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1));
    let env = bridge_protocol::Envelope {
        feature: ECHO,
        kind: 1,
        payload: bridge_protocol::encode_payload(&Note(666)).unwrap(),
    };
    b.core
        .handle_frame(Micros(2), link.cb, ChannelClass::Clipboard, &env.to_bytes());
    let out = pump(&mut a, &mut b, &link, Micros(2));
    assert!(
        out.closed
            .iter()
            .any(|(_, c, r)| *c == CloseCode::PROTOCOL_VIOLATION && r == "evil number")
    );
    assert!(
        a.log
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e.contains("down") && e.contains("Closed"))
    );
}

#[test]
fn handshake_times_out() {
    let mut b = node(2, PROTOCOL_VERSION, None);
    b.core.handle_connected(
        Micros(0),
        info(2, Role::Responder, DeviceId([1; 32]), true, &[]),
    );
    assert_eq!(b.core.next_deadline(), Some(Micros(0) + HANDSHAKE_TIMEOUT));
    b.core.handle_tick(Micros(0) + Duration::from_secs(4));
    assert!(b.core.poll_action().is_none());
    b.core.handle_tick(Micros(0) + HANDSHAKE_TIMEOUT);
    assert!(
        matches!(b.core.poll_action(), Some(Action::Close { reason, .. }) if reason == "handshake timeout")
    );
}

#[test]
fn leaving_close_reports_left_and_lost_without_code() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1));
    a.core
        .handle_closed(Micros(2), link.ca, Some(CloseCode::LEAVING));
    assert!(matches!(
        std::iter::from_fn(|| a.core.poll_action()).last(),
        Some(Action::Event(CoreEvent::PeerDown {
            reason: PeerDownReason::Left,
            ..
        }))
    ));
    b.core.handle_closed(Micros(2), link.cb, None);
    assert!(
        b.log
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e.ends_with("Lost"))
    );
}

#[test]
fn duplicate_connections_keep_the_one_dialled_by_the_smaller_id() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    // B (larger id) dials first, then A dials: A's connection must win.
    let (_l1, _) = connect(&mut b, &mut a, 10, true, Micros(1));
    let (l2, out) = connect(&mut a, &mut b, 20, true, Micros(2));
    assert_eq!(a.core.peers().len(), 1);
    assert_eq!(b.core.peers().len(), 1);
    assert!(
        out.closed
            .iter()
            .any(|(_, c, r)| *c == CloseCode::NORMAL && r.contains("replaced"))
    );

    // A third connection dialled by B now loses against A's.
    let (_, out3) = connect(&mut b, &mut a, 30, true, Micros(3));
    assert!(
        out3.closed
            .iter()
            .any(|(_, _, r)| r == "duplicate connection")
    );
    assert_eq!(a.core.peers().len(), 1);
    let _ = l2;
}

#[test]
fn clock_pings_produce_estimates_and_device_status_is_forwarded() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1_000));
    a.core.handle_tick(Micros(2_000));
    let out = pump(&mut a, &mut b, &link, Micros(2_400));
    let est = a.core.clock(&b.id).expect("estimate after one exchange");
    assert_eq!(est.samples, 1);
    assert!(
        out.events_a
            .iter()
            .any(|e| matches!(e, CoreEvent::Clock { .. }))
    );
    assert!(a.core.next_deadline().is_some());

    b.core
        .set_local_state(Micros(3_000), DeviceState::Paused, StateReason::USER);
    let out = pump(&mut a, &mut b, &link, Micros(3_000));
    assert!(out.events_a.iter().any(|e| matches!(
        e,
        CoreEvent::PeerState {
            state: DeviceState::Paused,
            ..
        }
    )));
}

#[test]
fn revoke_sends_unpaired_and_closes() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(1));
    let b_id = b.id;
    a.core.revoke(&b_id);
    let out = pump(&mut a, &mut b, &link, Micros(2));
    assert!(
        out.events_b
            .iter()
            .any(|e| matches!(e, CoreEvent::Unpaired { .. }))
    );
    assert!(out.closed.iter().any(|(_, c, _)| *c == CloseCode::REVOKED));
    assert!(
        b.log
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e.ends_with("Revoked"))
    );
}

#[test]
fn registry_rejects_reserved_duplicate_and_empty_features() {
    struct Bad(FeatureId, std::ops::RangeInclusive<u16>);
    impl Feature for Bad {
        fn info(&self) -> FeatureInfo {
            FeatureInfo {
                id: self.0,
                name: "bad",
                versions: self.1.clone(),
                channel: ChannelClass::Control,
                permission: None,
            }
        }
        fn on_peer_up(&mut self, _: &mut FeatureCx<'_>, _: PeerId, _: u16) {}
        fn on_peer_down(&mut self, _: &mut FeatureCx<'_>, _: PeerId, _: PeerDownReason) {}
        fn on_message(
            &mut self,
            _: &mut FeatureCx<'_>,
            _: PeerId,
            _: u16,
            _: &[u8],
        ) -> Result<(), FeatureError> {
            Ok(())
        }
    }
    let mut r = FeatureRegistry::new();
    assert_eq!(
        r.register(Box::new(Bad(FeatureId::CORE, 1..=1))),
        Err(RegistryError::Reserved(FeatureId::CORE))
    );
    #[allow(clippy::reversed_empty_ranges)]
    let empty = 2..=1;
    assert_eq!(
        r.register(Box::new(Bad(FeatureId(5), empty))),
        Err(RegistryError::EmptyVersions(FeatureId(5)))
    );
    assert!(r.register(Box::new(Bad(FeatureId(5), 1..=1))).is_ok());
    assert_eq!(
        r.register(Box::new(Bad(FeatureId(5), 1..=1))),
        Err(RegistryError::Duplicate(FeatureId(5)))
    );
    assert_eq!(r.len(), 1);
    assert!(!r.is_empty());
}

#[test]
fn layouts_converge_with_both_devices_screens() {
    let mut a = node(1, PROTOCOL_VERSION, None);
    let mut b = node(2, PROTOCOL_VERSION, None);
    let screen = |id| LocalScreen {
        id,
        px_w: 1920,
        px_h: 1080,
        origin_x: 0,
        origin_y: 0,
        scale_permille: 1000,
        size_mm: None,
    };
    a.core.set_local_screens(Micros(1), vec![screen(0)]);
    b.core.set_local_screens(Micros(1), vec![screen(0)]);
    let (link, _) = connect(&mut a, &mut b, 1, true, Micros(2));
    pump(&mut a, &mut b, &link, Micros(3));
    let la = a.core.layout().unwrap().clone();
    let lb = b.core.layout().unwrap().clone();
    assert_eq!(la, lb, "same layout on both sides");
    assert_eq!(la.screens.len(), 2);
    let xa = la
        .screens
        .iter()
        .find(|s| s.device == a.id)
        .unwrap()
        .rect_mm
        .x;
    let xb = la
        .screens
        .iter()
        .find(|s| s.device == b.id)
        .unwrap()
        .rect_mm
        .x;
    assert!(xa != xb, "devices do not overlap");

    // A resolution change on B propagates as a newer version.
    let mut big = screen(0);
    big.px_w = 3840;
    big.px_h = 2160;
    b.core.set_local_screens(Micros(4), vec![big]);
    pump(&mut a, &mut b, &link, Micros(4));
    let la2 = a.core.layout().unwrap();
    assert!(la2.version > la.version);
    assert_eq!(
        la2.screens
            .iter()
            .find(|s| s.device == b.id)
            .unwrap()
            .px_width,
        3840
    );

    // The editor moves A's screen; both see it.
    let mut placed = la2.screens.to_vec();
    placed
        .iter_mut()
        .find(|s| s.device == a.id)
        .unwrap()
        .rect_mm
        .y = 50.0;
    a.core.set_layout(Micros(5), placed).unwrap();
    pump(&mut a, &mut b, &link, Micros(5));
    assert_eq!(
        b.core
            .layout()
            .unwrap()
            .screens
            .iter()
            .find(|s| s.device == a.id)
            .unwrap()
            .rect_mm
            .y,
        50.0
    );
    let mut bad = b.core.layout().unwrap().screens.to_vec();
    bad[0].rect_mm.w = f64::NAN;
    assert!(
        a.core.set_layout(Micros(6), bad).is_err(),
        "editor input is validated"
    );
}
