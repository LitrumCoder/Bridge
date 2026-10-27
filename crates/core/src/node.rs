//! The sans-IO hub of a device: per-connection handshake, peer table, routing of
//! envelopes to features and clock sync. Drivers feed it events and execute the
//! actions it returns (ADR-0001).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use bridge_protocol::control::{
    ClientHello, ClockPing, ClockPong, ControlMessage, DeviceState, DeviceStatus, Intent,
    LayoutDigest, LayoutRequest, LayoutUpdate, LayoutVersion, PairConfirm, PairStart, Permissions,
    Reject, ScreenPlacement, ServerHello, StateReason, Unpaired,
};
use bridge_protocol::{DecodeError, Validate};

use crate::layout::{LocalScreen, with_own_screens};
use bridge_protocol::{
    BoundedString, BoundedVec, ChannelClass, CloseCode, DeviceId, Envelope, FeatureId,
    NegotiatedFeature, PermissionId, Platform, ProtocolVersion, SessionToken, check_compatibility,
    decode_datagram, encode_datagram, negotiate_features,
};

use crate::clock::{ClockEstimate, ClockSample, ClockSync};
use crate::feature::{FeatureCx, FeatureRegistry, Outgoing, PeerDownReason, PeerId, PeerView};
use crate::pairing::{
    self, Entropy, PairingError, PairingKey, PairingSecret, PairingWindow, Pake, Side,
};
use crate::{ChannelScheduler, Micros};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Pairing adds two round trips and runs while a person waits.
pub const PAIRING_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOCK_FAST_INTERVAL: Duration = Duration::from_millis(200);
const CLOCK_FAST_PINGS: u32 = 25;
const CLOCK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub device_id: DeviceId,
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub protocol: ProtocolVersion,
    /// Permissions a freshly paired device gets (own devices: everything).
    pub default_grants: Vec<PermissionId>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ConnId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Initiator,
    Responder,
}

/// What the driver knows about a fresh, TLS-authenticated connection.
#[derive(Clone, Debug)]
pub struct ConnectInfo {
    pub conn: ConnId,
    pub role: Role,
    /// Identity proven by the peer's certificate.
    pub peer: DeviceId,
    /// Whether the peer is in our trust store.
    pub trusted: bool,
    /// What we allow this peer to do to us.
    pub granted: Vec<PermissionId>,
    /// Initiator only: pair with this secret instead of a normal hello.
    pub pair: Option<PairingSecret>,
    /// Opaque session id chosen by the driver for `ServerHello`.
    pub session: SessionToken,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub protocol: ProtocolVersion,
    pub features: Vec<NegotiatedFeature>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum CoreEvent {
    PeerUp(PeerInfo),
    PeerDown {
        peer: PeerId,
        reason: PeerDownReason,
    },
    PeerState {
        peer: PeerId,
        state: DeviceState,
        reason: StateReason,
    },
    PeerPermissions {
        peer: PeerId,
        granted: Vec<PermissionId>,
    },
    /// The peer refused our connection.
    Rejected {
        conn: ConnId,
        peer: PeerId,
        code: CloseCode,
        message: String,
        protocol: ProtocolVersion,
    },
    Clock {
        peer: PeerId,
        estimate: ClockEstimate,
    },
    Unpaired {
        peer: PeerId,
    },
    /// Pairing succeeded; the driver persists the peer in its trust store.
    Paired {
        conn: ConnId,
        peer: PeerId,
        role: Role,
    },
    PairingFailed {
        conn: ConnId,
        peer: PeerId,
        reason: String,
    },
    /// The shared screen layout changed (locally or from a peer).
    LayoutChanged(LayoutUpdate),
}

#[derive(Clone, PartialEq, Debug)]
pub enum Action {
    /// Write one envelope as a frame on the stream of `channel`.
    Send {
        conn: ConnId,
        channel: ChannelClass,
        envelope: Vec<u8>,
    },
    SendDatagram {
        conn: ConnId,
        bytes: Vec<u8>,
    },
    Close {
        conn: ConnId,
        code: CloseCode,
        reason: String,
    },
    Event(CoreEvent),
}

/// Counters for things dropped without closing the connection.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CoreStats {
    pub unknown_feature: u64,
    pub not_permitted: u64,
    pub feature_errors: u64,
    pub violations: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    AwaitServerHello,
    AwaitClientHello,
    /// Responder: got `ClientHello` with `Intent::Pair`, waits for the SPAKE2 message.
    AwaitPairStart,
    /// Initiator: sent its SPAKE2 message, waits for the responder's.
    AwaitPairReply,
    /// Both: keys derived, waiting for the other side's confirmation.
    AwaitPairConfirm,
    Active,
    Closing,
}

struct Session {
    info: ConnectInfo,
    phase: Phase,
    deadline: Option<Micros>,
    peer: Option<PeerInfo>,
    remote_grants: Vec<PermissionId>,
    remote_state: DeviceState,
    clock: ClockSync,
    next_ping: Micros,
    pings_sent: u32,
    /// Feature traffic that overtook `ServerHello` on other streams (initiator only).
    early: Vec<Early>,
    early_bytes: usize,
    pake: Option<Pake>,
    pair_key: Option<PairingKey>,
    /// Responder: the hello to continue with once pairing succeeds.
    pending_hello: Option<ClientHello>,
}

enum Early {
    Frame(ChannelClass, Vec<u8>),
    Datagram(Vec<u8>),
}

/// The responder activates one round trip before the initiator, and QUIC does not order
/// streams against each other, so a little feature traffic may arrive "early".
const EARLY_LIMIT_BYTES: usize = 256 * 1024;
const EARLY_LIMIT_ITEMS: usize = 256;

impl Session {
    fn buffer_early(&mut self, item: Early) -> bool {
        let len = match &item {
            Early::Frame(_, b) | Early::Datagram(b) => b.len(),
        };
        if self.early.len() >= EARLY_LIMIT_ITEMS || self.early_bytes + len > EARLY_LIMIT_BYTES {
            return false;
        }
        self.early_bytes += len;
        self.early.push(item);
        true
    }

    fn negotiated(&self, feature: FeatureId) -> Option<u16> {
        self.peer
            .as_ref()?
            .features
            .iter()
            .find(|f| f.id == feature)
            .map(|f| f.version)
    }

    fn initiator(&self, us: DeviceId) -> DeviceId {
        match self.info.role {
            Role::Initiator => us,
            Role::Responder => self.info.peer,
        }
    }
}

pub struct Core {
    config: CoreConfig,
    registry: FeatureRegistry,
    scheduler: ChannelScheduler,
    sessions: BTreeMap<ConnId, Session>,
    primary: HashMap<PeerId, ConnId>,
    local_state: (DeviceState, StateReason),
    actions: VecDeque<Action>,
    outgoing: Vec<Outgoing>,
    stats: CoreStats,
    pairing: PairingWindow,
    entropy: Box<dyn Entropy>,
    layout: Option<LayoutUpdate>,
    local_screens: Vec<LocalScreen>,
    lamport: u64,
}

impl Core {
    pub fn new(config: CoreConfig, registry: FeatureRegistry, entropy: Box<dyn Entropy>) -> Self {
        Self {
            config,
            registry,
            scheduler: ChannelScheduler::default(),
            sessions: BTreeMap::new(),
            primary: HashMap::new(),
            local_state: (DeviceState::Connected, StateReason::USER),
            actions: VecDeque::new(),
            outgoing: Vec::new(),
            stats: CoreStats::default(),
            pairing: PairingWindow::default(),
            entropy,
            layout: None,
            local_screens: Vec::new(),
            lamport: 0,
        }
    }

    pub fn layout(&self) -> Option<&LayoutUpdate> {
        self.layout.as_ref()
    }

    /// This device's monitors changed (start, resolution, hot-plug).
    pub fn set_local_screens(&mut self, now: Micros, screens: Vec<LocalScreen>) {
        self.local_screens = screens;
        let merged = with_own_screens(
            self.layout.as_ref(),
            self.config.device_id,
            &self.local_screens,
        );
        if self
            .layout
            .as_ref()
            .is_none_or(|l| *l.screens != merged[..])
        {
            self.publish_layout(now, merged);
        }
    }

    /// A new arrangement from the layout editor.
    pub fn set_layout(
        &mut self,
        now: Micros,
        screens: Vec<ScreenPlacement>,
    ) -> Result<(), DecodeError> {
        let screens =
            BoundedVec::try_from_vec(screens).map_err(|_| DecodeError::TooLong { max: 64 })?;
        let candidate = LayoutUpdate {
            version: LayoutVersion {
                counter: 0,
                device: self.config.device_id,
            },
            screens,
        };
        candidate.validate()?;
        self.publish_layout(now, candidate.screens.into_vec());
        Ok(())
    }

    fn publish_layout(&mut self, now: Micros, screens: Vec<ScreenPlacement>) {
        self.lamport += 1;
        let update = LayoutUpdate {
            version: LayoutVersion {
                counter: self.lamport,
                device: self.config.device_id,
            },
            screens: BoundedVec::try_from_vec(screens).unwrap_or_default(),
        };
        let active: Vec<ConnId> = self.primary.values().copied().collect();
        for conn in active {
            self.send_control(conn, update.clone().into());
        }
        self.layout_changed(now, update);
    }

    fn layout_changed(&mut self, now: Micros, update: LayoutUpdate) {
        self.layout = Some(update.clone());
        self.actions
            .push_back(Action::Event(CoreEvent::LayoutChanged(update.clone())));
        for id in self.registry.ids() {
            self.dispatch_simple(now, id, |f, cx| f.on_layout(cx, &update));
        }
    }

    fn adopt_layout(&mut self, now: Micros, update: LayoutUpdate) {
        if self
            .layout
            .as_ref()
            .is_some_and(|l| l.version >= update.version)
        {
            return;
        }
        self.lamport = self.lamport.max(update.version.counter);
        let merged = with_own_screens(Some(&update), self.config.device_id, &self.local_screens);
        if self.local_screens.is_empty() || *update.screens == merged[..] {
            self.layout_changed(now, update);
        } else {
            // The peer's layout lacks our current screens: add them as a newer version.
            self.layout = Some(update);
            self.publish_layout(now, merged);
        }
    }

    /// An event from outside the network (e.g. the input hook) for one feature.
    pub fn handle_local(
        &mut self,
        now: Micros,
        feature: FeatureId,
        event: Box<dyn std::any::Any + Send>,
    ) {
        self.dispatch_simple(now, feature, |f, cx| f.on_local(cx, event));
    }

    /// Opens the pairing window and returns the fresh code and QR secret to show.
    pub fn open_pairing(
        &mut self,
        now: Micros,
        duration: Duration,
    ) -> Result<(PairingSecret, PairingSecret), PairingError> {
        let code = PairingSecret::generate_code(self.entropy.as_mut());
        let qr = PairingSecret::generate_qr(self.entropy.as_mut());
        self.pairing.open(now, code.clone(), qr.clone(), duration)?;
        Ok((code, qr))
    }

    pub fn close_pairing(&mut self) {
        self.pairing.close();
    }

    pub fn pairing_open(&self, now: Micros) -> bool {
        self.pairing.is_open(now)
    }

    pub fn pairing_expires(&self) -> Option<Micros> {
        self.pairing.expires()
    }

    pub fn pairing_locked_until(&self, now: Micros) -> Option<Micros> {
        self.pairing.locked_until(now)
    }

    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    pub fn scheduler(&self) -> &ChannelScheduler {
        &self.scheduler
    }

    pub fn stats(&self) -> CoreStats {
        self.stats
    }

    pub fn local_state(&self) -> DeviceState {
        self.local_state.0
    }

    pub fn poll_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    /// Active peers (one entry per device, never duplicates).
    pub fn peers(&self) -> Vec<PeerInfo> {
        let mut out: Vec<PeerInfo> = self
            .primary
            .values()
            .filter_map(|c| self.sessions.get(c)?.peer.clone())
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    pub fn clock(&self, peer: &PeerId) -> Option<ClockEstimate> {
        self.sessions.get(self.primary.get(peer)?)?.clock.estimate()
    }

    /// Converts a timestamp of `peer`'s clock into ours.
    pub fn to_local_time(&self, peer: &PeerId, remote: u64) -> Option<u64> {
        self.sessions
            .get(self.primary.get(peer)?)?
            .clock
            .to_local(remote)
    }

    /// Earliest time at which `handle_tick` has work to do.
    pub fn next_deadline(&self) -> Option<Micros> {
        self.sessions
            .values()
            .filter_map(|s| match s.phase {
                Phase::Active => Some(s.next_ping),
                Phase::Closing => None,
                _ => s.deadline,
            })
            .min()
    }

    pub fn handle_connected(&mut self, now: Micros, info: ConnectInfo) {
        let conn = info.conn;
        let role = info.role;
        let session = Session {
            phase: match role {
                Role::Initiator => Phase::AwaitServerHello,
                Role::Responder => Phase::AwaitClientHello,
            },
            deadline: Some(now + HANDSHAKE_TIMEOUT),
            peer: None,
            remote_grants: Vec::new(),
            remote_state: DeviceState::Connected,
            clock: ClockSync::new(),
            next_ping: now,
            pings_sent: 0,
            early: Vec::new(),
            early_bytes: 0,
            pake: None,
            pair_key: None,
            pending_hello: None,
            info,
        };
        let pair = session.info.pair.clone();
        let peer = session.info.peer;
        self.sessions.insert(conn, session);
        if role != Role::Initiator {
            return;
        }
        let hello = ClientHello {
            protocol: self.config.protocol,
            app_version: BoundedString::truncating(&self.config.app_version),
            device_id: self.config.device_id,
            name: BoundedString::truncating(&self.config.name),
            platform: self.config.platform,
            features: BoundedVec::try_from_vec(self.registry.descriptors()).unwrap_or_default(),
            intent: if pair.is_some() {
                Intent::Pair
            } else {
                Intent::Normal
            },
            resume: None,
        };
        self.send_control(conn, hello.into());
        if let Some(secret) = pair {
            let us = self.config.device_id;
            let (pake, msg) =
                pairing::start(Side::Initiator, &secret, us, peer, self.entropy.as_mut());
            if let Some(s) = self.sessions.get_mut(&conn) {
                s.pake = Some(pake);
                s.phase = Phase::AwaitPairReply;
                s.deadline = Some(now + PAIRING_HANDSHAKE_TIMEOUT);
            }
            let start = PairStart {
                method: secret.method(),
                spake: BoundedVec::try_from_vec(msg).unwrap_or_default(),
            };
            self.send_control(conn, start.into());
        }
    }

    pub fn handle_frame(&mut self, now: Micros, conn: ConnId, channel: ChannelClass, bytes: &[u8]) {
        let Some(phase) = self.sessions.get(&conn).map(|s| s.phase) else {
            return;
        };
        if phase == Phase::Closing {
            return;
        }
        let env = match Envelope::parse(bytes) {
            Ok(env) => env,
            Err(e) => return self.violation(conn, &format!("bad envelope: {e}")),
        };
        if env.feature == FeatureId::CORE {
            if channel != ChannelClass::Control {
                return self.violation(conn, "core message outside the control stream");
            }
            match ControlMessage::from_envelope(&env) {
                Ok(msg) => self.handle_control(now, conn, msg),
                Err(e) => self.violation(conn, &format!("bad control message: {e}")),
            }
            return;
        }
        let initiator_waiting = phase != Phase::Active
            && self
                .sessions
                .get(&conn)
                .is_some_and(|s| s.info.role == Role::Initiator);
        if initiator_waiting {
            let buffered = self
                .sessions
                .get_mut(&conn)
                .is_some_and(|s| s.buffer_early(Early::Frame(channel, bytes.to_vec())));
            if !buffered {
                self.violation(conn, "too much feature traffic before handshake");
            }
            return;
        }
        if phase != Phase::Active {
            return self.violation(conn, "feature message before handshake");
        }
        if let Some(peer) = self.routable_peer(conn, env.feature) {
            self.dispatch(
                now,
                env.feature,
                |f, cx| f.on_message(cx, peer, env.kind, env.payload),
                conn,
            );
        }
    }

    pub fn handle_datagram(&mut self, now: Micros, conn: ConnId, bytes: &[u8]) {
        match self.sessions.get_mut(&conn) {
            Some(s) if s.phase == Phase::Active => {}
            Some(s) if s.info.role == Role::Initiator && s.phase != Phase::Closing => {
                // Datagrams are unreliable anyway: past the limit they are just dropped.
                s.buffer_early(Early::Datagram(bytes.to_vec()));
                return;
            }
            _ => return,
        }
        let Ok((feature, payload)) = decode_datagram(bytes) else {
            self.stats.feature_errors += 1;
            return;
        };
        if let Some(peer) = self.routable_peer(conn, feature) {
            self.dispatch(now, feature, |f, cx| f.on_datagram(cx, peer, payload), conn);
        }
    }

    /// The transport reports that a connection is gone (closed by either side).
    pub fn handle_closed(&mut self, now: Micros, conn: ConnId, code: Option<CloseCode>) {
        let Some(session) = self.sessions.remove(&conn) else {
            return;
        };
        let peer = session.info.peer;
        if self.primary.get(&peer) != Some(&conn) {
            return;
        }
        self.primary.remove(&peer);
        let reason = match code {
            Some(CloseCode::LEAVING) => PeerDownReason::Left,
            Some(CloseCode::REVOKED) => PeerDownReason::Revoked,
            None => PeerDownReason::Lost,
            Some(_) => PeerDownReason::Closed,
        };
        self.peer_down(now, peer, session.peer.as_ref(), reason);
    }

    pub fn handle_tick(&mut self, now: Micros) {
        let conns: Vec<ConnId> = self.sessions.keys().copied().collect();
        for conn in conns {
            let Some(s) = self.sessions.get_mut(&conn) else {
                continue;
            };
            match s.phase {
                Phase::AwaitClientHello
                | Phase::AwaitServerHello
                | Phase::AwaitPairStart
                | Phase::AwaitPairReply
                | Phase::AwaitPairConfirm => {
                    if s.deadline.is_some_and(|d| now >= d) {
                        self.close(conn, CloseCode::PROTOCOL_VIOLATION, "handshake timeout");
                    }
                }
                Phase::Active if now >= s.next_ping => {
                    s.pings_sent += 1;
                    let interval = if s.pings_sent <= CLOCK_FAST_PINGS {
                        CLOCK_FAST_INTERVAL
                    } else {
                        CLOCK_INTERVAL
                    };
                    s.next_ping = now + interval;
                    self.send_control(conn, ClockPing { t0: now.0 }.into());
                }
                _ => {}
            }
        }
        for id in self.registry.ids() {
            self.dispatch_simple(now, id, |f, cx| f.on_tick(cx));
        }
    }

    pub fn set_local_state(&mut self, now: Micros, state: DeviceState, reason: StateReason) {
        self.local_state = (state, reason);
        let active: Vec<ConnId> = self.primary.values().copied().collect();
        for conn in active {
            self.send_control(conn, DeviceStatus { state, reason }.into());
        }
        for id in self.registry.ids() {
            self.dispatch_simple(now, id, |f, cx| f.on_device_state(cx, state));
        }
    }

    /// Changes what `peer` may do to us; applies to live connections immediately.
    pub fn set_grants(&mut self, peer: &PeerId, granted: Vec<PermissionId>) {
        let conns: Vec<ConnId> = self
            .sessions
            .iter_mut()
            .filter(|(_, s)| s.info.peer == *peer)
            .map(|(c, s)| {
                s.info.granted = granted.clone();
                *c
            })
            .collect();
        for conn in conns {
            if self
                .sessions
                .get(&conn)
                .is_some_and(|s| s.phase == Phase::Active)
            {
                let msg = Permissions {
                    granted: BoundedVec::try_from_vec(granted.clone()).unwrap_or_default(),
                };
                self.send_control(conn, msg.into());
            }
        }
    }

    /// Tells the peer we no longer trust it and closes every connection to it.
    pub fn revoke(&mut self, peer: &PeerId) {
        let conns: Vec<ConnId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.info.peer == *peer)
            .map(|(c, _)| *c)
            .collect();
        for conn in conns {
            self.send_control(conn, Unpaired.into());
            self.close(conn, CloseCode::REVOKED, "pairing revoked");
        }
    }

    /// Closes a connection on behalf of the driver (shutdown, leaving).
    pub fn close(&mut self, conn: ConnId, code: CloseCode, reason: &str) {
        if let Some(s) = self.sessions.get_mut(&conn) {
            if s.phase == Phase::Closing {
                return;
            }
            s.phase = Phase::Closing;
            self.actions.push_back(Action::Close {
                conn,
                code,
                reason: reason.to_owned(),
            });
        }
    }

    fn handle_control(&mut self, now: Micros, conn: ConnId, msg: ControlMessage) {
        let Some(phase) = self.sessions.get(&conn).map(|s| s.phase) else {
            return;
        };
        match (phase, msg) {
            (Phase::AwaitClientHello, ControlMessage::ClientHello(h)) => {
                self.on_client_hello(now, conn, h)
            }
            (Phase::AwaitServerHello, ControlMessage::ServerHello(h)) => {
                self.on_server_hello(now, conn, h)
            }
            (Phase::AwaitPairStart, ControlMessage::PairStart(p)) => {
                self.on_pair_start_responder(now, conn, p)
            }
            (Phase::AwaitPairReply, ControlMessage::PairStart(p)) => {
                self.on_pair_start_initiator(conn, p)
            }
            (Phase::AwaitPairConfirm, ControlMessage::PairConfirm(c)) => {
                self.on_pair_confirm(now, conn, c)
            }
            (
                Phase::AwaitServerHello
                | Phase::AwaitPairReply
                | Phase::AwaitPairConfirm
                | Phase::Active,
                ControlMessage::Reject(r),
            ) => {
                if let Some(s) = self.sessions.get(&conn) {
                    self.actions.push_back(Action::Event(CoreEvent::Rejected {
                        conn,
                        peer: s.info.peer,
                        code: r.code,
                        message: r.message.to_string(),
                        protocol: r.protocol,
                    }));
                }
                self.close(conn, r.code, "rejected by peer");
            }
            (Phase::Active, ControlMessage::DeviceStatus(st)) => {
                if let Some(s) = self.sessions.get_mut(&conn) {
                    s.remote_state = st.state;
                }
                if let Some(peer) = self.active_primary_peer(conn) {
                    self.actions.push_back(Action::Event(CoreEvent::PeerState {
                        peer,
                        state: st.state,
                        reason: st.reason,
                    }));
                    self.peer_updated(now, conn, peer);
                }
            }
            (Phase::Active, ControlMessage::Permissions(p)) => {
                if let Some(s) = self.sessions.get_mut(&conn) {
                    s.remote_grants = p.granted.to_vec();
                    let peer = s.info.peer;
                    self.actions
                        .push_back(Action::Event(CoreEvent::PeerPermissions {
                            peer,
                            granted: p.granted.into_vec(),
                        }));
                    if self.active_primary_peer(conn) == Some(peer) {
                        self.peer_updated(now, conn, peer);
                    }
                }
            }
            (Phase::Active, ControlMessage::ClockPing(p)) => {
                self.send_control(
                    conn,
                    ClockPong {
                        t0: p.t0,
                        t1: now.0,
                        t2: now.0,
                    }
                    .into(),
                );
            }
            (Phase::Active, ControlMessage::ClockPong(p)) => {
                let Some(s) = self.sessions.get_mut(&conn) else {
                    return;
                };
                if let Some(sample) = ClockSample::from_exchange(p.t0, p.t1, p.t2, now.0) {
                    s.clock.add(sample);
                    if let Some(estimate) = s.clock.estimate() {
                        let peer = s.info.peer;
                        self.actions
                            .push_back(Action::Event(CoreEvent::Clock { peer, estimate }));
                    }
                }
            }
            (Phase::Active, ControlMessage::LayoutDigest(d)) => match &self.layout {
                None => self.send_control(conn, LayoutRequest.into()),
                Some(l) if l.version > d.version => self.send_control(conn, l.clone().into()),
                Some(l) if l.version < d.version => self.send_control(conn, LayoutRequest.into()),
                Some(_) => {}
            },
            (Phase::Active, ControlMessage::LayoutRequest(_)) => {
                if let Some(l) = self.layout.clone() {
                    self.send_control(conn, l.into());
                }
            }
            (Phase::Active, ControlMessage::LayoutUpdate(u)) => self.adopt_layout(now, u),
            (Phase::Active, ControlMessage::Unpaired(_)) => {
                if let Some(s) = self.sessions.get(&conn) {
                    let peer = s.info.peer;
                    self.actions
                        .push_back(Action::Event(CoreEvent::Unpaired { peer }));
                }
                self.close(conn, CloseCode::REVOKED, "peer revoked the pairing");
            }
            _ => self.violation(conn, "unexpected control message"),
        }
    }

    fn on_client_hello(&mut self, now: Micros, conn: ConnId, h: ClientHello) {
        let Some(s) = self.sessions.get(&conn) else {
            return;
        };
        if h.device_id != s.info.peer {
            return self.violation(conn, "hello identity does not match the certificate");
        }
        if check_compatibility(self.config.protocol, h.protocol).is_err() {
            let msg = format!(
                "protocol {} is not compatible with {}",
                h.protocol, self.config.protocol
            );
            return self.reject(conn, CloseCode::INCOMPATIBLE_VERSION, &msg);
        }
        if h.intent == Intent::Pair {
            if let Some(until) = self.pairing.locked_until(now) {
                let secs = until.saturating_sub(now).as_secs();
                let msg = format!("pairing is locked for {secs} s after repeated failures");
                return self.reject(conn, CloseCode::PAIRING_FAILED, &msg);
            }
            if !self.pairing.is_open(now) {
                return self.reject(
                    conn,
                    CloseCode::PAIRING_FAILED,
                    "pairing is not open on this device",
                );
            }
            if let Some(s) = self.sessions.get_mut(&conn) {
                s.pending_hello = Some(h);
                s.phase = Phase::AwaitPairStart;
                s.deadline = Some(now + PAIRING_HANDSHAKE_TIMEOUT);
            }
            return;
        }
        if !s.info.trusted {
            return self.reject(conn, CloseCode::NOT_PAIRED, "this device is not paired");
        }
        self.accept_hello(now, conn, h);
    }

    fn on_pair_start_responder(&mut self, now: Micros, conn: ConnId, p: PairStart) {
        let Some(secret) = self.pairing.secret(now, p.method) else {
            return self.reject(
                conn,
                CloseCode::PAIRING_FAILED,
                "pairing is not open on this device",
            );
        };
        let Some(peer) = self.sessions.get(&conn).map(|s| s.info.peer) else {
            return;
        };
        let us = self.config.device_id;
        let (pake, msg) = pairing::start(Side::Responder, &secret, peer, us, self.entropy.as_mut());
        let Some(key) = pake.finish(&p.spake) else {
            self.pairing.record_failure(now);
            return self.pairing_failed(conn, peer, "malformed pairing message");
        };
        if let Some(s) = self.sessions.get_mut(&conn) {
            s.pair_key = Some(key);
            s.phase = Phase::AwaitPairConfirm;
        }
        let reply = PairStart {
            method: p.method,
            spake: BoundedVec::try_from_vec(msg).unwrap_or_default(),
        };
        self.send_control(conn, reply.into());
    }

    fn on_pair_start_initiator(&mut self, conn: ConnId, p: PairStart) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        let peer = s.info.peer;
        let Some(key) = s.pake.take().and_then(|pake| pake.finish(&p.spake)) else {
            return self.pairing_failed(conn, peer, "malformed pairing message");
        };
        let mac = key.confirmation(Side::Initiator);
        s.pair_key = Some(key);
        s.phase = Phase::AwaitPairConfirm;
        self.send_control(conn, PairConfirm { mac }.into());
    }

    fn on_pair_confirm(&mut self, now: Micros, conn: ConnId, c: PairConfirm) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        let peer = s.info.peer;
        let role = s.info.role;
        let Some(key) = s.pair_key.take() else { return };
        match role {
            Role::Responder => {
                if !key.verify(Side::Initiator, &c.mac) {
                    self.pairing.record_failure(now);
                    return self.pairing_failed(conn, peer, "the code does not match");
                }
                self.pairing.record_success();
                let mac = key.confirmation(Side::Responder);
                self.send_control(conn, PairConfirm { mac }.into());
                let hello = self.sessions.get_mut(&conn).and_then(|s| {
                    s.info.trusted = true;
                    s.info.granted = self.config.default_grants.clone();
                    s.pending_hello.take()
                });
                self.actions
                    .push_back(Action::Event(CoreEvent::Paired { conn, peer, role }));
                if let Some(h) = hello {
                    self.accept_hello(now, conn, h);
                }
            }
            Role::Initiator => {
                if !key.verify(Side::Responder, &c.mac) {
                    return self.pairing_failed(
                        conn,
                        peer,
                        "the other device could not prove the code",
                    );
                }
                s.info.trusted = true;
                s.phase = Phase::AwaitServerHello;
                self.actions
                    .push_back(Action::Event(CoreEvent::Paired { conn, peer, role }));
            }
        }
    }

    fn pairing_failed(&mut self, conn: ConnId, peer: PeerId, reason: &str) {
        self.actions
            .push_back(Action::Event(CoreEvent::PairingFailed {
                conn,
                peer,
                reason: reason.to_owned(),
            }));
        self.reject(conn, CloseCode::PAIRING_FAILED, reason);
    }

    /// Answers a hello from a trusted device and activates the session.
    fn accept_hello(&mut self, now: Micros, conn: ConnId, h: ClientHello) {
        let Some(s) = self.sessions.get(&conn) else {
            return;
        };
        let negotiated = negotiate_features(&self.registry.descriptors(), &h.features);
        let reply = ServerHello {
            protocol: self.config.protocol,
            app_version: BoundedString::truncating(&self.config.app_version),
            device_id: self.config.device_id,
            name: BoundedString::truncating(&self.config.name),
            platform: self.config.platform,
            features: BoundedVec::try_from_vec(self.registry.descriptors()).unwrap_or_default(),
            negotiated: BoundedVec::try_from_vec(negotiated.clone()).unwrap_or_default(),
            session: s.info.session,
        };
        self.send_control(conn, reply.into());
        let peer = PeerInfo {
            id: h.device_id,
            name: h.name.to_string(),
            platform: h.platform,
            app_version: h.app_version.to_string(),
            protocol: h.protocol,
            features: negotiated,
        };
        self.activate(now, conn, peer);
    }

    fn on_server_hello(&mut self, now: Micros, conn: ConnId, h: ServerHello) {
        let Some(s) = self.sessions.get(&conn) else {
            return;
        };
        if h.device_id != s.info.peer {
            return self.violation(conn, "hello identity does not match the certificate");
        }
        if check_compatibility(self.config.protocol, h.protocol).is_err() {
            return self.close(conn, CloseCode::INCOMPATIBLE_VERSION, "incompatible server");
        }
        let expected = negotiate_features(&self.registry.descriptors(), &h.features);
        if expected.as_slice() != &*h.negotiated {
            return self.violation(conn, "negotiated features do not match");
        }
        let peer = PeerInfo {
            id: h.device_id,
            name: h.name.to_string(),
            platform: h.platform,
            app_version: h.app_version.to_string(),
            protocol: h.protocol,
            features: expected,
        };
        self.activate(now, conn, peer);
    }

    fn activate(&mut self, now: Micros, conn: ConnId, peer: PeerInfo) {
        let us = self.config.device_id;
        let peer_id = peer.id;
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        s.phase = Phase::Active;
        s.deadline = None;
        s.next_ping = now;
        s.peer = Some(peer.clone());
        let granted = s.info.granted.clone();
        let new_initiator = s.initiator(us);

        let (state, reason) = self.local_state;
        self.send_control(conn, DeviceStatus { state, reason }.into());
        self.send_control(
            conn,
            Permissions {
                granted: BoundedVec::try_from_vec(granted).unwrap_or_default(),
            }
            .into(),
        );

        if let Some(&old) = self.primary.get(&peer_id) {
            let preferred = us.min(peer_id);
            let old_initiator = self.sessions.get(&old).map(|s| s.initiator(us));
            let old_wins = old_initiator == Some(preferred) && new_initiator != preferred;
            if old_wins {
                return self.close(conn, CloseCode::NORMAL, "duplicate connection");
            }
            let old_peer = self.sessions.get(&old).and_then(|s| s.peer.clone());
            self.primary.remove(&peer_id);
            self.peer_down(now, peer_id, old_peer.as_ref(), PeerDownReason::Replaced);
            self.close(old, CloseCode::NORMAL, "replaced by a newer connection");
        }
        self.primary.insert(peer_id, conn);
        self.actions
            .push_back(Action::Event(CoreEvent::PeerUp(peer.clone())));
        for f in &peer.features {
            let version = f.version;
            self.dispatch_simple(now, f.id, |feat, cx| feat.on_peer_up(cx, peer_id, version));
        }
        if let Some(l) = &self.layout {
            let digest = LayoutDigest { version: l.version };
            self.send_control(conn, digest.into());
        }
        let early = self
            .sessions
            .get_mut(&conn)
            .map(|s| {
                s.early_bytes = 0;
                std::mem::take(&mut s.early)
            })
            .unwrap_or_default();
        for item in early {
            match item {
                Early::Frame(channel, bytes) => self.handle_frame(now, conn, channel, &bytes),
                Early::Datagram(bytes) => self.handle_datagram(now, conn, &bytes),
            }
        }
    }

    fn peer_down(
        &mut self,
        now: Micros,
        peer: PeerId,
        info: Option<&PeerInfo>,
        reason: PeerDownReason,
    ) {
        self.actions
            .push_back(Action::Event(CoreEvent::PeerDown { peer, reason }));
        for f in info.map(|i| i.features.clone()).unwrap_or_default() {
            self.dispatch_simple(now, f.id, |feat, cx| feat.on_peer_down(cx, peer, reason));
        }
    }

    fn peer_updated(&mut self, now: Micros, conn: ConnId, peer: PeerId) {
        let features: Vec<FeatureId> = self
            .sessions
            .get(&conn)
            .and_then(|s| s.peer.as_ref())
            .map(|p| p.features.iter().map(|f| f.id).collect())
            .unwrap_or_default();
        for f in features {
            self.dispatch_simple(now, f, |feat, cx| feat.on_peer_update(cx, peer));
        }
    }

    fn active_primary_peer(&self, conn: ConnId) -> Option<PeerId> {
        let s = self.sessions.get(&conn)?;
        (self.primary.get(&s.info.peer) == Some(&conn)).then_some(s.info.peer)
    }

    /// Checks negotiation and our permission for a feature message on `conn`.
    fn routable_peer(&mut self, conn: ConnId, feature: FeatureId) -> Option<PeerId> {
        let peer = self.active_primary_peer(conn)?;
        let s = self.sessions.get(&conn)?;
        let Some(info) = self.registry.info(feature) else {
            self.stats.unknown_feature += 1;
            return None;
        };
        if s.negotiated(feature).is_none() {
            self.stats.unknown_feature += 1;
            return None;
        }
        if let Some(p) = info.permission
            && !s.info.granted.contains(&p)
        {
            self.stats.not_permitted += 1;
            return None;
        }
        Some(peer)
    }

    fn views(&self, feature: FeatureId) -> Vec<PeerView> {
        let permission = self.registry.info(feature).and_then(|i| i.permission);
        let mut views: Vec<PeerView> = self
            .primary
            .values()
            .filter_map(|c| {
                let s = self.sessions.get(c)?;
                if s.phase != Phase::Active {
                    return None;
                }
                Some(PeerView {
                    id: s.info.peer,
                    version: s.negotiated(feature)?,
                    allows_us: permission.is_none_or(|p| s.remote_grants.contains(&p)),
                    state: s.remote_state,
                    platform: s.peer.as_ref().map_or(Platform::UNKNOWN, |p| p.platform),
                    clock_offset: s.clock.estimate().map(|e| e.offset),
                })
            })
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }

    fn dispatch_simple(
        &mut self,
        now: Micros,
        feature: FeatureId,
        call: impl FnOnce(&mut dyn crate::Feature, &mut FeatureCx<'_>),
    ) {
        let views = self.views(feature);
        let mut out = std::mem::take(&mut self.outgoing);
        if let Some((_, f)) = self.registry.get_mut(feature) {
            let mut cx = FeatureCx {
                now,
                feature,
                peers: &views,
                out: &mut out,
            };
            call(f, &mut cx);
        }
        self.outgoing = out;
        self.flush_outgoing();
    }

    fn dispatch(
        &mut self,
        now: Micros,
        feature: FeatureId,
        call: impl FnOnce(
            &mut dyn crate::Feature,
            &mut FeatureCx<'_>,
        ) -> Result<(), crate::FeatureError>,
        conn: ConnId,
    ) {
        let views = self.views(feature);
        let mut out = std::mem::take(&mut self.outgoing);
        let result = match self.registry.get_mut(feature) {
            Some((_, f)) => {
                let mut cx = FeatureCx {
                    now,
                    feature,
                    peers: &views,
                    out: &mut out,
                };
                call(f, &mut cx)
            }
            None => Ok(()),
        };
        self.outgoing = out;
        self.flush_outgoing();
        match result {
            Ok(()) => {}
            Err(crate::FeatureError::Violation(why)) => self.violation(conn, &why),
            Err(crate::FeatureError::Decode(e)) => self.violation(conn, &e.to_string()),
            Err(crate::FeatureError::Other(_)) => self.stats.feature_errors += 1,
        }
    }

    fn flush_outgoing(&mut self) {
        for o in std::mem::take(&mut self.outgoing) {
            let Some(&conn) = self.primary.get(&o.peer) else {
                continue;
            };
            if o.datagram {
                let bytes = encode_datagram(o.feature, &o.payload);
                self.actions.push_back(Action::SendDatagram { conn, bytes });
            } else {
                let channel = self
                    .registry
                    .info(o.feature)
                    .map_or(ChannelClass::Control, |i| i.channel);
                let envelope = Envelope {
                    feature: o.feature,
                    kind: o.kind,
                    payload: o.payload,
                }
                .to_bytes();
                self.actions.push_back(Action::Send {
                    conn,
                    channel,
                    envelope,
                });
            }
        }
    }

    fn send_control(&mut self, conn: ConnId, msg: ControlMessage) {
        // Our own control messages are bounded by construction and always encode.
        if let Ok(env) = msg.to_envelope() {
            self.actions.push_back(Action::Send {
                conn,
                channel: ChannelClass::Control,
                envelope: env.to_bytes(),
            });
        }
    }

    fn reject(&mut self, conn: ConnId, code: CloseCode, message: &str) {
        let r = Reject {
            code,
            protocol: self.config.protocol,
            message: BoundedString::truncating(message),
        };
        self.send_control(conn, r.into());
        self.close(conn, code, message);
    }

    fn violation(&mut self, conn: ConnId, why: &str) {
        self.stats.violations += 1;
        self.close(conn, CloseCode::PROTOCOL_VIOLATION, why);
    }
}
