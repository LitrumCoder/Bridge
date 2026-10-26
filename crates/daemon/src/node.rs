//! Async driver around the sans-IO [`Core`]: owns connections and streams, feeds the
//! core with frames and time, executes its actions (ARCHITECTURE §2.2, ADR-0001).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::{
    Action, ConnId, ConnectInfo, Core, CoreConfig, CoreEvent, Feature, FeatureRegistry, Micros,
    PeerInfo, RegistryError, Role,
};
use bridge_protocol::control::{DeviceState, Intent};
use bridge_protocol::{
    ChannelClass, CloseCode, DeviceId, FrameDecoder, PROTOCOL_VERSION, PermissionId, Platform,
    SessionToken, encode_frame,
};
use bridge_transport::{
    CloseInfo, ClosedBy, Connection, ExpectedPeer, PeerAddr, QuicOptions, QuicTransport,
    RecvStream, SendStream, Transport, TransportError, TrustPolicy,
};
use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use bridge_ipc::{NodeStatus, PeerStatus};

const MAX_BACKOFF: Duration = Duration::from_secs(5);
const STABLE_CONNECTION: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct TrustedPeer {
    pub id: DeviceId,
    /// What we allow this peer to do to us.
    pub granted: Vec<PermissionId>,
}

impl TrustedPeer {
    /// Own devices get everything by default (ARCHITECTURE §9).
    pub fn own_device(id: DeviceId) -> Self {
        Self {
            id,
            granted: vec![
                PermissionId::INPUT,
                PermissionId::FILES,
                PermissionId::CLIPBOARD,
                PermissionId::MIDI,
            ],
        }
    }
}

/// In-memory trust store; persisted by the caller. TODO(M3): pairing and persistence.
#[derive(Default)]
pub struct TrustStore {
    peers: Mutex<HashMap<DeviceId, TrustedPeer>>,
}

impl TrustStore {
    pub fn new(peers: impl IntoIterator<Item = TrustedPeer>) -> Arc<Self> {
        Arc::new(Self {
            peers: Mutex::new(peers.into_iter().map(|p| (p.id, p)).collect()),
        })
    }

    pub fn get(&self, id: &DeviceId) -> Option<TrustedPeer> {
        self.lock().get(id).cloned()
    }

    pub fn insert(&self, peer: TrustedPeer) {
        self.lock().insert(peer.id, peer);
    }

    pub fn remove(&self, id: &DeviceId) -> bool {
        self.lock().remove(id).is_some()
    }

    pub fn all(&self) -> Vec<TrustedPeer> {
        let mut v: Vec<TrustedPeer> = self.lock().values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DeviceId, TrustedPeer>> {
        self.peers.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl TrustPolicy for TrustStore {
    fn is_trusted(&self, id: &DeviceId) -> bool {
        self.lock().contains_key(id)
    }
}

#[derive(Clone, Debug)]
pub enum NodeEvent {
    Core(CoreEvent),
    /// A dial target gave up for good (not paired, revoked, incompatible).
    DialStopped {
        addr: SocketAddr,
        peer: DeviceId,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("the node has stopped")]
    Stopped,
}

pub struct NodeBuilder {
    name: String,
    registry: FeatureRegistry,
    trust: Arc<TrustStore>,
    protocol: bridge_protocol::ProtocolVersion,
}

impl NodeBuilder {
    pub fn new(name: impl Into<String>, trust: Arc<TrustStore>) -> Self {
        Self {
            name: name.into(),
            registry: FeatureRegistry::new(),
            trust,
            protocol: PROTOCOL_VERSION,
        }
    }

    /// Overrides the advertised protocol version (compatibility tests only).
    pub fn protocol_version(mut self, version: bridge_protocol::ProtocolVersion) -> Self {
        self.protocol = version;
        self
    }

    pub fn feature(mut self, feature: impl Feature) -> Result<Self, RegistryError> {
        self.registry.register(Box::new(feature))?;
        Ok(self)
    }

    /// Starts the node on an already bound transport. Must run inside a tokio runtime.
    pub fn start(self, transport: QuicTransport) -> Result<NodeHandle, NodeError> {
        let device_id = transport.identity().id();
        let PeerAddr::Ip(listen) = transport.local_addr()?;
        let core = Core::new(
            CoreConfig {
                device_id,
                name: self.name.clone(),
                platform: Platform::current(),
                app_version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: self.protocol,
            },
            self.registry,
        );
        let transport = Arc::new(transport);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(256);
        let (connected_tx, connected_rx) = watch::channel(HashSet::new());
        let status = Arc::new(Mutex::new(NodeStatus {
            device_id,
            name: self.name,
            listen,
            state: DeviceState::Connected,
            peers: Vec::new(),
        }));
        let handle = NodeHandle {
            inner: Arc::new(Inner {
                input: input_tx.clone(),
                events: events.clone(),
                connected: connected_rx,
                status: status.clone(),
                transport: transport.clone(),
                trust: self.trust.clone(),
                next_conn: Mutex::new(0),
            }),
        };
        let driver = Driver {
            core,
            start: Instant::now(),
            input: input_tx,
            conns: HashMap::new(),
            events,
            connected: connected_tx,
            status,
            trust: self.trust,
        };
        tokio::spawn(driver.run(input_rx));
        tokio::spawn(accept_loop(handle.clone()));
        Ok(handle)
    }
}

struct Inner {
    input: mpsc::UnboundedSender<Input>,
    events: broadcast::Sender<NodeEvent>,
    connected: watch::Receiver<HashSet<DeviceId>>,
    status: Arc<Mutex<NodeStatus>>,
    transport: Arc<QuicTransport>,
    trust: Arc<TrustStore>,
    next_conn: Mutex<u64>,
}

/// Cheap, cloneable handle to a running node.
#[derive(Clone)]
pub struct NodeHandle {
    inner: Arc<Inner>,
}

impl NodeHandle {
    pub fn device_id(&self) -> DeviceId {
        self.inner.transport.identity().id()
    }

    pub fn status(&self) -> NodeStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn trust(&self) -> &Arc<TrustStore> {
        &self.inner.trust
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.inner.events.subscribe()
    }

    pub fn is_connected(&self, peer: &DeviceId) -> bool {
        self.inner.connected.borrow().contains(peer)
    }

    /// Waits until `peer` is (or is not) connected, up to `timeout`.
    pub async fn wait_connected(&self, peer: DeviceId, connected: bool, timeout: Duration) -> bool {
        let mut rx = self.inner.connected.clone();
        tokio::time::timeout(timeout, rx.wait_for(|set| set.contains(&peer) == connected))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Keeps a connection to `peer` at `addr`, redialling with backoff while it is lost.
    pub fn dial(&self, addr: SocketAddr, peer: DeviceId) {
        tokio::spawn(dial_loop(self.clone(), addr, peer));
    }

    pub fn revoke(&self, peer: DeviceId) {
        self.inner.trust.remove(&peer);
        let _ = self.inner.input.send(Input::Revoke(peer));
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.inner.input.send(Input::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        self.inner.transport.close();
    }

    fn next_conn(&self) -> ConnId {
        let mut n = self
            .inner
            .next_conn
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *n += 1;
        ConnId(*n)
    }

    /// Opens or accepts the control stream and hands the connection to the driver.
    async fn attach(
        &self,
        conn: Arc<dyn Connection>,
        role: Role,
    ) -> Result<(ConnId, oneshot::Receiver<CloseInfo>), TransportError> {
        let priority = 100;
        let (mut send, mut recv) = match role {
            Role::Initiator => {
                let (mut s, r) = conn.open_bi(priority).await?;
                s.write_all(&[ChannelClass::Control.stream_tag()]).await?;
                (s, r)
            }
            Role::Responder => {
                let (s, mut r) = conn.accept_bi().await?;
                let mut tag = [0u8; 1];
                read_exact(&mut r, &mut tag).await?;
                if tag[0] != ChannelClass::Control.stream_tag() {
                    conn.close(
                        CloseCode::PROTOCOL_VIOLATION,
                        "first stream must be control",
                    );
                    return Err(TransportError::Other("bad control stream".into()));
                }
                (s, r)
            }
        };
        let id = self.next_conn();
        let (closed_tx, closed_rx) = oneshot::channel();
        let input = self.inner.input.clone();
        let (writer, writer_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move { write_frames(&mut *send, writer_rx).await });
        let input2 = input.clone();
        tokio::spawn(
            async move { read_frames(id, ChannelClass::Control, &mut *recv, input2).await },
        );
        input
            .send(Input::Connected {
                id,
                conn,
                role,
                control: writer,
                closed: closed_tx,
            })
            .map_err(|_| TransportError::Other("node stopped".into()))?;
        Ok((id, closed_rx))
    }
}

async fn accept_loop(node: NodeHandle) {
    while let Some(result) = node.inner.transport.accept().await {
        match result {
            Ok(conn) => {
                let node = node.clone();
                tokio::spawn(async move {
                    let peer = conn.peer();
                    if let Err(e) = node.attach(conn, Role::Responder).await {
                        debug!(%peer, "incoming connection failed: {e}");
                    }
                });
            }
            Err(e) => debug!("incoming handshake failed: {e}"),
        }
    }
}

async fn dial_loop(node: NodeHandle, addr: SocketAddr, peer: DeviceId) {
    let mut backoff = Duration::from_millis(250);
    loop {
        // Never race a connection the peer already has with us.
        let mut connected = node.inner.connected.clone();
        if connected
            .wait_for(|set| !set.contains(&peer))
            .await
            .is_err()
        {
            return;
        }
        if node.inner.input.is_closed() {
            return;
        }
        let started = Instant::now();
        let result = node
            .inner
            .transport
            .connect(
                PeerAddr::Ip(addr),
                ExpectedPeer::Device(peer),
                bridge_core::ConnectionKind::Interactive,
            )
            .await;
        let stop = match result {
            Ok(conn) => match node.attach(conn, Role::Initiator).await {
                Ok((_, closed)) => match closed.await {
                    Ok(info) => permanent(&info),
                    Err(_) => return,
                },
                Err(e) => {
                    debug!(%addr, "attach failed: {e}");
                    None
                }
            },
            Err(TransportError::NotPaired) => {
                Some("the peer does not trust this device".to_owned())
            }
            Err(TransportError::WrongPeer) => {
                Some("another device answered at this address".to_owned())
            }
            Err(e) => {
                debug!(%addr, "dial failed: {e}");
                None
            }
        };
        if let Some(reason) = stop {
            info!(%addr, %peer, "giving up: {reason}");
            let _ = node
                .inner
                .events
                .send(NodeEvent::DialStopped { addr, peer, reason });
            return;
        }
        if started.elapsed() > STABLE_CONNECTION {
            backoff = Duration::from_millis(250);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn permanent(info: &CloseInfo) -> Option<String> {
    match info.code {
        Some(CloseCode::NOT_PAIRED) => Some("the peer does not trust this device".into()),
        Some(CloseCode::REVOKED) => Some("the pairing was revoked".into()),
        Some(CloseCode::INCOMPATIBLE_VERSION) => {
            Some(format!("incompatible version: {}", info.reason))
        }
        _ => None,
    }
}

async fn read_exact(r: &mut Box<dyn RecvStream>, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]).await? {
            Some(n) => got += n,
            None => return Err(TransportError::Other("stream ended early".into())),
        }
    }
    Ok(())
}

async fn write_frames(send: &mut dyn SendStream, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if send.write_all(&frame).await.is_err() {
            return;
        }
    }
    let _ = send.finish();
}

async fn read_frames(
    conn: ConnId,
    channel: ChannelClass,
    recv: &mut dyn RecvStream,
    input: mpsc::UnboundedSender<Input>,
) {
    let mut decoder = FrameDecoder::new(channel.max_frame_len());
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => decoder.push(&buf[..n]),
            Ok(None) | Err(_) => return,
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if input
                        .send(Input::Frame {
                            conn,
                            channel,
                            bytes: frame,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = input.send(Input::Violation {
                        conn,
                        reason: e.to_string(),
                    });
                    return;
                }
            }
        }
    }
}

async fn accept_streams(
    conn_id: ConnId,
    conn: Arc<dyn Connection>,
    input: mpsc::UnboundedSender<Input>,
) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let input = input.clone();
        tokio::spawn(async move {
            let mut tag = [0u8; 1];
            if read_exact(&mut recv, &mut tag).await.is_err() {
                return;
            }
            match ChannelClass::from_stream_tag(tag[0]) {
                Some(class) if class != ChannelClass::Control => {
                    read_frames(conn_id, class, &mut *recv, input).await;
                }
                _ => {
                    let reason = format!("unexpected stream tag {}", tag[0]);
                    let _ = input.send(Input::Violation {
                        conn: conn_id,
                        reason,
                    });
                }
            }
        });
    }
}

async fn read_datagrams(
    conn_id: ConnId,
    conn: Arc<dyn Connection>,
    input: mpsc::UnboundedSender<Input>,
) {
    let limit = ChannelClass::InputMotion.max_frame_len() as usize;
    while let Ok(d) = conn.recv_datagram().await {
        if d.len() > limit {
            continue;
        }
        if input
            .send(Input::Datagram {
                conn: conn_id,
                bytes: d,
            })
            .is_err()
        {
            return;
        }
    }
}

enum Input {
    Connected {
        id: ConnId,
        conn: Arc<dyn Connection>,
        role: Role,
        control: mpsc::UnboundedSender<Vec<u8>>,
        closed: oneshot::Sender<CloseInfo>,
    },
    Frame {
        conn: ConnId,
        channel: ChannelClass,
        bytes: Vec<u8>,
    },
    Datagram {
        conn: ConnId,
        bytes: Bytes,
    },
    Violation {
        conn: ConnId,
        reason: String,
    },
    Closed {
        conn: ConnId,
        info: CloseInfo,
    },
    Revoke(DeviceId),
    Shutdown(oneshot::Sender<()>),
}

struct ConnEntry {
    conn: Arc<dyn Connection>,
    writers: HashMap<ChannelClass, mpsc::UnboundedSender<Vec<u8>>>,
    closed: Option<oneshot::Sender<CloseInfo>>,
}

struct Driver {
    core: Core,
    start: Instant,
    input: mpsc::UnboundedSender<Input>,
    conns: HashMap<ConnId, ConnEntry>,
    events: broadcast::Sender<NodeEvent>,
    connected: watch::Sender<HashSet<DeviceId>>,
    status: Arc<Mutex<NodeStatus>>,
    trust: Arc<TrustStore>,
}

impl Driver {
    fn now(&self) -> Micros {
        Micros::from_duration(self.start.elapsed())
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        loop {
            let deadline = self
                .core
                .next_deadline()
                .map(|d| self.start + d.as_duration())
                .unwrap_or_else(|| Instant::now() + Duration::from_millis(250));
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return };
                    if let Input::Shutdown(done) = msg {
                        self.shutdown();
                        let _ = done.send(());
                        return;
                    }
                    self.handle(msg);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = self.now();
                    self.core.handle_tick(now);
                }
            }
            self.execute();
        }
    }

    fn handle(&mut self, msg: Input) {
        let now = self.now();
        match msg {
            Input::Connected {
                id,
                conn,
                role,
                control,
                closed,
            } => {
                let peer = conn.peer();
                let trusted = self.trust.get(&peer);
                let mut writers = HashMap::new();
                writers.insert(ChannelClass::Control, control);
                tokio::spawn(accept_streams(id, conn.clone(), self.input.clone()));
                tokio::spawn(read_datagrams(id, conn.clone(), self.input.clone()));
                let input = self.input.clone();
                let watch_conn = conn.clone();
                tokio::spawn(async move {
                    let info = watch_conn.closed().await;
                    let _ = input.send(Input::Closed { conn: id, info });
                });
                self.conns.insert(
                    id,
                    ConnEntry {
                        conn,
                        writers,
                        closed: Some(closed),
                    },
                );
                let mut session = [0u8; 16];
                session[..8].copy_from_slice(&id.0.to_le_bytes());
                self.core.handle_connected(
                    now,
                    ConnectInfo {
                        conn: id,
                        role,
                        peer,
                        trusted: trusted.is_some(),
                        granted: trusted.map(|t| t.granted).unwrap_or_default(),
                        intent: Intent::Normal,
                        session: SessionToken(session),
                    },
                );
            }
            Input::Frame {
                conn,
                channel,
                bytes,
            } => self.core.handle_frame(now, conn, channel, &bytes),
            Input::Datagram { conn, bytes } => self.core.handle_datagram(now, conn, &bytes),
            Input::Violation { conn, reason } => {
                self.core
                    .close(conn, CloseCode::PROTOCOL_VIOLATION, &reason);
            }
            Input::Closed { conn, info } => {
                let code = match info.by {
                    ClosedBy::Transport => None,
                    _ => info.code,
                };
                self.core.handle_closed(now, conn, code);
                if let Some(mut entry) = self.conns.remove(&conn)
                    && let Some(tx) = entry.closed.take()
                {
                    let _ = tx.send(info);
                }
            }
            Input::Revoke(peer) => self.core.revoke(&peer),
            Input::Shutdown(_) => {}
        }
    }

    fn execute(&mut self) {
        let mut peers_changed = false;
        while let Some(action) = self.core.poll_action() {
            match action {
                Action::Send {
                    conn,
                    channel,
                    envelope,
                } => self.send(conn, channel, envelope),
                Action::SendDatagram { conn, bytes } => {
                    if let Some(e) = self.conns.get(&conn) {
                        let _ = e.conn.send_datagram(Bytes::from(bytes));
                    }
                }
                Action::Close { conn, code, reason } => {
                    if let Some(e) = self.conns.get(&conn) {
                        debug!(peer = %e.conn.peer(), ?code, "closing: {reason}");
                        e.conn.close(code, &reason);
                    }
                }
                Action::Event(event) => {
                    peers_changed |=
                        matches!(event, CoreEvent::PeerUp(_) | CoreEvent::PeerDown { .. });
                    if matches!(event, CoreEvent::Clock { .. }) {
                        peers_changed = true;
                    }
                    if let CoreEvent::Rejected { code, message, .. } = &event {
                        warn!(?code, "connection rejected: {message}");
                    }
                    let _ = self.events.send(NodeEvent::Core(event));
                }
            }
        }
        if peers_changed {
            self.refresh_status();
        }
    }

    fn send(&mut self, conn: ConnId, channel: ChannelClass, envelope: Vec<u8>) {
        let Some(entry) = self.conns.get_mut(&conn) else {
            return;
        };
        let mut frame = Vec::with_capacity(envelope.len() + 4);
        if encode_frame(&envelope, channel.max_frame_len(), &mut frame).is_err() {
            warn!(?channel, len = envelope.len(), "dropping oversized frame");
            return;
        }
        let writer = entry.writers.entry(channel).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let c = entry.conn.clone();
            let priority = self.core.scheduler().plan(channel).priority;
            tokio::spawn(async move {
                let Ok(mut send) = c.open_uni(priority).await else {
                    return;
                };
                if send.write_all(&[channel.stream_tag()]).await.is_err() {
                    return;
                }
                write_frames(&mut *send, rx).await;
            });
            tx
        });
        let _ = writer.send(frame);
    }

    fn refresh_status(&mut self) {
        let peers: Vec<PeerInfo> = self.core.peers();
        let set: HashSet<DeviceId> = peers.iter().map(|p| p.id).collect();
        self.connected.send_replace(set);
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.state = self.core.local_state();
        status.peers = peers
            .into_iter()
            .map(|p| {
                let clock = self.core.clock(&p.id);
                PeerStatus {
                    id: p.id,
                    name: p.name,
                    platform: p.platform,
                    app_version: p.app_version,
                    rtt_us: clock.map(|c| c.min_rtt),
                    clock_offset_us: clock.map(|c| c.offset),
                }
            })
            .collect();
    }

    fn shutdown(&mut self) {
        for e in self.conns.values() {
            e.conn.close(CloseCode::NORMAL, "shutdown");
        }
    }
}

/// Binds the QUIC transport for a node with production defaults.
pub fn bind_transport(
    addr: SocketAddr,
    identity: bridge_transport::DeviceIdentity,
    trust: Arc<TrustStore>,
) -> Result<QuicTransport, TransportError> {
    QuicTransport::bind(addr, identity, trust, QuicOptions::default())
}
