//! Async driver around the sans-IO [`Core`]: owns connections and streams, feeds the
//! core with frames and time, executes its actions (ARCHITECTURE §2.2, ADR-0001).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::{
    Action, ConnId, ConnectInfo, Core, CoreConfig, CoreEvent, Entropy, Feature, FeatureRegistry,
    Micros, PairingSecret, PeerInfo, RegistryError, Role,
};
use bridge_discovery::{AdState, Advertisement, Discovered, Discovery, DiscoveryEvent};
use bridge_ipc::{NodeStatus, PeerStatus};
use bridge_protocol::control::DeviceState;
use bridge_protocol::{
    ChannelClass, CloseCode, DeviceId, FrameDecoder, PROTOCOL_VERSION, PermissionId, Platform,
    ProtocolVersion, SessionToken, encode_frame,
};
use bridge_transport::{
    CloseInfo, ClosedBy, Connection, ExpectedPeer, PeerAddr, QuicOptions, QuicTransport,
    RecvStream, SendStream, Transport, TransportError,
};
use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::pairing_uri::PairingInvite;
use crate::trust::{ALL_PERMISSIONS, TrustStore, TrustedPeer};

const MAX_BACKOFF: Duration = Duration::from_secs(5);
const STABLE_CONNECTION: Duration = Duration::from_secs(10);
const PAIR_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub enum NodeEvent {
    Core(CoreEvent),
    /// A dial target gave up for good (not paired, revoked, incompatible).
    DialStopped {
        addr: SocketAddr,
        peer: DeviceId,
        reason: String,
    },
    Nearby(DiscoveryEvent),
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("the node has stopped")]
    Stopped,
    #[error("{0}")]
    Pairing(String),
}

/// OS randomness for the core (pairing codes, SPAKE2 scalars).
struct OsEntropy(ring::rand::SystemRandom);

impl Entropy for OsEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        ring::rand::SecureRandom::fill(&self.0, buf)
            .expect("the OS random number generator failed");
    }
}

/// Lets code outside the network loop (an OS hook thread, the UI) hand events to one
/// feature; they arrive in `Feature::on_local`.
#[derive(Clone)]
pub struct LocalSender {
    feature: bridge_protocol::FeatureId,
    input: mpsc::UnboundedSender<Input>,
}

impl LocalSender {
    pub fn post(&self, event: impl std::any::Any + Send) {
        let _ = self.input.send(Input::Local {
            feature: self.feature,
            event: Box::new(event),
        });
    }
}

pub struct NodeBuilder {
    name: String,
    registry: FeatureRegistry,
    trust: Arc<TrustStore>,
    protocol: ProtocolVersion,
    discovery: Option<Arc<dyn Discovery>>,
    hide_name: bool,
    entropy: Option<Box<dyn Entropy>>,
    screens: Vec<bridge_core::LocalScreen>,
    input_tx: mpsc::UnboundedSender<Input>,
    input_rx: mpsc::UnboundedReceiver<Input>,
}

impl NodeBuilder {
    pub fn new(name: impl Into<String>, trust: Arc<TrustStore>) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        Self {
            name: name.into(),
            registry: FeatureRegistry::new(),
            trust,
            protocol: PROTOCOL_VERSION,
            discovery: None,
            hide_name: false,
            entropy: None,
            screens: Vec::new(),
            input_tx,
            input_rx,
        }
    }

    /// A channel into `feature` that can be created before the node starts.
    pub fn local_sender(&self, feature: bridge_protocol::FeatureId) -> LocalSender {
        LocalSender {
            feature,
            input: self.input_tx.clone(),
        }
    }

    /// This device's monitors, written into the shared layout at start.
    pub fn screens(mut self, screens: Vec<bridge_core::LocalScreen>) -> Self {
        self.screens = screens;
        self
    }

    pub fn feature(mut self, feature: impl Feature) -> Result<Self, RegistryError> {
        self.registry.register(Box::new(feature))?;
        Ok(self)
    }

    pub fn discovery(mut self, discovery: Arc<dyn Discovery>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// Announce only the fingerprint, not the device name (privacy setting).
    pub fn hide_name(mut self, hide: bool) -> Self {
        self.hide_name = hide;
        self
    }

    /// Overrides the advertised protocol version (compatibility tests only).
    pub fn protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.protocol = version;
        self
    }

    /// Replaces OS randomness (deterministic tests only).
    pub fn entropy(mut self, entropy: Box<dyn Entropy>) -> Self {
        self.entropy = Some(entropy);
        self
    }

    /// Starts the node on an already bound transport. Must run inside a tokio runtime.
    pub fn start(self, transport: QuicTransport) -> Result<NodeHandle, NodeError> {
        let device_id = transport.identity().id();
        let PeerAddr::Ip(listen) = transport.local_addr()?;
        let entropy = self
            .entropy
            .unwrap_or_else(|| Box::new(OsEntropy(ring::rand::SystemRandom::new())));
        let mut core = Core::new(
            CoreConfig {
                device_id,
                name: self.name.clone(),
                platform: Platform::current(),
                app_version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: self.protocol,
                default_grants: ALL_PERMISSIONS.to_vec(),
            },
            self.registry,
            entropy,
        );
        if !self.screens.is_empty() {
            core.set_local_screens(Micros::ZERO, self.screens.clone());
        }
        let transport = Arc::new(transport);
        let (input_tx, input_rx) = (self.input_tx, self.input_rx);
        let (events, _) = broadcast::channel(256);
        let (connected_tx, connected_rx) = watch::channel(HashSet::new());
        let status = Arc::new(Mutex::new(NodeStatus {
            device_id,
            name: self.name.clone(),
            listen,
            state: DeviceState::Connected,
            peers: Vec::new(),
            layout: core.layout().cloned(),
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
                dials: Mutex::new(HashMap::new()),
                nearby: Mutex::new(HashMap::new()),
                discovery: self.discovery.clone(),
                listen,
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
            trust: self.trust.clone(),
        };
        tokio::spawn(driver.run(input_rx));
        tokio::spawn(accept_loop(handle.clone()));

        if let Some(d) = &self.discovery {
            let ad = Advertisement {
                id: device_id,
                name: (!self.hide_name).then(|| self.name.clone()),
                port: listen.port(),
                state: AdState::Connected,
            };
            if let Err(e) = d.advertise(&ad) {
                warn!("cannot announce this device: {e}");
            }
            tokio::spawn(discovery_loop(handle.clone(), d.subscribe()));
        }
        for peer in self.trust.all() {
            if let Some(addr) = peer.addrs.first() {
                handle.dial(*addr, peer.id);
            }
        }
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
    dials: Mutex<HashMap<DeviceId, (SocketAddr, JoinHandle<()>)>>,
    nearby: Mutex<HashMap<DeviceId, Discovered>>,
    discovery: Option<Arc<dyn Discovery>>,
    listen: SocketAddr,
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

    pub fn listen_addr(&self) -> SocketAddr {
        self.inner.listen
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

    /// Devices currently announced on the local network (paired or not).
    pub fn nearby(&self) -> Vec<Discovered> {
        let mut v: Vec<Discovered> = self
            .inner
            .nearby
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|d| d.id);
        v
    }

    /// Waits until `peer` is (or is not) connected, up to `timeout`.
    pub async fn wait_connected(&self, peer: DeviceId, connected: bool, timeout: Duration) -> bool {
        let mut rx = self.inner.connected.clone();
        tokio::time::timeout(timeout, rx.wait_for(|set| set.contains(&peer) == connected))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Keeps a connection to `peer` at `addr`, redialling with backoff while it is lost.
    /// A second call for the same peer replaces the address.
    pub fn dial(&self, addr: SocketAddr, peer: DeviceId) {
        let mut dials = self.inner.dials.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((old, task)) = dials.get(&peer) {
            if *old == addr && !task.is_finished() {
                return;
            }
            task.abort();
        }
        let task = tokio::spawn(dial_loop(self.clone(), addr, peer));
        dials.insert(peer, (addr, task));
    }

    fn stop_dial(&self, peer: &DeviceId) {
        if let Some((_, task)) = self
            .inner
            .dials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(peer)
        {
            task.abort();
        }
    }

    /// Forgets `peer`: removes it from the trust store, tells it, closes connections.
    pub fn unpair(&self, peer: DeviceId) -> bool {
        self.stop_dial(&peer);
        let known = self.inner.trust.remove(&peer);
        let _ = self.inner.input.send(Input::Revoke(peer));
        known
    }

    pub fn set_permissions(&self, peer: DeviceId, granted: Vec<PermissionId>) -> bool {
        let g = granted.clone();
        let known = self.inner.trust.update(&peer, |p| p.granted = g);
        if known {
            let _ = self.inner.input.send(Input::SetGrants(peer, granted));
        }
        known
    }

    /// Opens the pairing window and returns what to show: the code and the QR URI.
    pub async fn open_pairing(&self) -> Result<PairingInvite, NodeError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .input
            .send(Input::OpenPairing(tx))
            .map_err(|_| NodeError::Stopped)?;
        let (code, qr, expires_in) = rx
            .await
            .map_err(|_| NodeError::Stopped)?
            .map_err(NodeError::Pairing)?;
        Ok(PairingInvite::new(
            self.device_id(),
            self.advertised_addr(),
            &code,
            &qr,
            expires_in,
        ))
    }

    pub fn close_pairing(&self) {
        let _ = self.inner.input.send(Input::ClosePairing);
    }

    /// The local monitors changed (resolution, hot-plug).
    pub fn set_screens(&self, screens: Vec<bridge_core::LocalScreen>) {
        let _ = self.inner.input.send(Input::SetScreens(screens));
    }

    /// The current shared screen layout.
    pub fn layout(&self) -> Option<bridge_protocol::control::LayoutUpdate> {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .layout
            .clone()
    }

    /// The address peers should dial: a LAN address of this machine with our port.
    fn advertised_addr(&self) -> SocketAddr {
        let listen = self.inner.listen;
        if !listen.ip().is_unspecified() {
            return listen;
        }
        // Routing lookup only; nothing is sent.
        std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").and_then(|_| s.local_addr()))
            .map(|a| SocketAddr::new(a.ip(), listen.port()))
            .unwrap_or(listen)
    }

    /// Pairs with the device at `addr` using a code or QR secret; on success the device
    /// is trusted and kept connected. Returns its id.
    pub async fn pair(
        &self,
        addr: SocketAddr,
        expect: ExpectedPeer,
        secret: PairingSecret,
    ) -> Result<DeviceId, NodeError> {
        let mut events = self.subscribe();
        let conn = self
            .inner
            .transport
            .connect(
                PeerAddr::Ip(addr),
                expect,
                bridge_core::ConnectionKind::Interactive,
            )
            .await?;
        let peer = conn.peer();
        let (conn_id, mut closed) = self.attach(conn, Role::Initiator, Some(secret)).await?;
        let outcome = tokio::time::timeout(PAIR_TIMEOUT, async {
            loop {
                let ev = tokio::select! {
                    ev = events.recv() => ev,
                    info = &mut closed => {
                        let why = info.map(|i| i.reason).unwrap_or_default();
                        return Err(if why.is_empty() { "the connection closed".to_owned() } else { why });
                    }
                };
                match ev {
                    Ok(NodeEvent::Core(CoreEvent::Paired { conn, peer, .. })) if conn == conn_id => {
                        return Ok(peer);
                    }
                    Ok(NodeEvent::Core(CoreEvent::PairingFailed { conn, reason, .. })) if conn == conn_id => {
                        return Err(reason);
                    }
                    Ok(NodeEvent::Core(CoreEvent::Rejected { conn, message, .. })) if conn == conn_id => {
                        return Err(message);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err("the node stopped".to_owned()),
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| NodeError::Pairing("pairing timed out".into()))?;
        let paired = outcome.map_err(NodeError::Pairing)?;
        debug_assert_eq!(paired, peer);
        self.dial(addr, paired);
        Ok(paired)
    }

    pub async fn shutdown(&self) {
        if let Some(d) = &self.inner.discovery {
            d.withdraw();
        }
        for (_, (_, task)) in self
            .inner
            .dials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
        {
            task.abort();
        }
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
        pair: Option<PairingSecret>,
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
        let (writer, writer_rx) = mpsc::unbounded_channel::<WriterMsg>();
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
                pair,
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
                    if let Err(e) = node.attach(conn, Role::Responder, None).await {
                        debug!(%peer, "incoming connection failed: {e}");
                    }
                });
            }
            Err(e) => debug!("incoming handshake failed: {e}"),
        }
    }
}

async fn discovery_loop(node: NodeHandle, mut events: mpsc::UnboundedReceiver<DiscoveryEvent>) {
    while let Some(ev) = events.recv().await {
        match &ev {
            DiscoveryEvent::Found(d) => {
                node.inner
                    .nearby
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(d.id, d.clone());
                let trusted = node.inner.trust.get(&d.id).is_some();
                if trusted
                    && d.state != AdState::Left
                    && !node.is_connected(&d.id)
                    && let Some(addr) = preferred_addr(&d.addrs)
                {
                    node.dial(addr, d.id);
                }
            }
            DiscoveryEvent::Lost(id) => {
                node.inner
                    .nearby
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(id);
            }
        }
        let _ = node.inner.events.send(NodeEvent::Nearby(ev));
    }
}

/// IPv4 LAN addresses first: link-local IPv6 needs a scope id that is easy to get wrong.
fn preferred_addr(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    addrs
        .iter()
        .find(|a| a.is_ipv4() && !a.ip().is_loopback())
        .or_else(|| addrs.iter().find(|a| !a.ip().is_loopback()))
        .or_else(|| addrs.first())
        .copied()
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
            Ok(conn) => match node.attach(conn, Role::Initiator, None).await {
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

enum WriterMsg {
    Frame(Vec<u8>),
    /// Close the connection after everything queued before it has been written:
    /// closing a QUIC connection discards unsent stream data, and a `Reject` or
    /// `Unpaired` right before the close must reach the peer.
    CloseAfter {
        conn: Arc<dyn Connection>,
        code: CloseCode,
        reason: String,
    },
}

const CLOSE_FLUSH_LIMIT: Duration = Duration::from_millis(500);

async fn write_frames(send: &mut dyn SendStream, mut rx: mpsc::UnboundedReceiver<WriterMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            WriterMsg::Frame(frame) => {
                if send.write_all(&frame).await.is_err() {
                    return;
                }
            }
            WriterMsg::CloseAfter { conn, code, reason } => {
                let _ = send.finish();
                // Give the peer a moment to acknowledge the last frames.
                let _ = tokio::time::timeout(CLOSE_FLUSH_LIMIT, async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                })
                .await;
                conn.close(code, &reason);
                return;
            }
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

type PairingReply = Result<(PairingSecret, PairingSecret, Duration), String>;

enum Input {
    Connected {
        id: ConnId,
        conn: Arc<dyn Connection>,
        role: Role,
        control: mpsc::UnboundedSender<WriterMsg>,
        closed: oneshot::Sender<CloseInfo>,
        pair: Option<PairingSecret>,
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
    SetGrants(DeviceId, Vec<PermissionId>),
    OpenPairing(oneshot::Sender<PairingReply>),
    ClosePairing,
    Local {
        feature: bridge_protocol::FeatureId,
        event: Box<dyn std::any::Any + Send>,
    },
    SetScreens(Vec<bridge_core::LocalScreen>),
    Shutdown(oneshot::Sender<()>),
}

struct ConnEntry {
    conn: Arc<dyn Connection>,
    writers: HashMap<ChannelClass, mpsc::UnboundedSender<WriterMsg>>,
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
            self.sync_pairing_window();
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
                pair,
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
                        pair,
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
            Input::SetGrants(peer, granted) => self.core.set_grants(&peer, granted),
            Input::OpenPairing(reply) => {
                let result = self
                    .core
                    .open_pairing(now, bridge_core::PAIRING_WINDOW)
                    .map(|(code, qr)| (code, qr, bridge_core::PAIRING_WINDOW))
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            Input::ClosePairing => self.core.close_pairing(),
            Input::Local { feature, event } => self.core.handle_local(now, feature, event),
            Input::SetScreens(screens) => self.core.set_local_screens(now, screens),
            Input::Shutdown(_) => {}
        }
    }

    /// The TLS verifier lets unknown certificates in only while the core's window is open.
    fn sync_pairing_window(&self) {
        let now = self.now();
        let until = self
            .core
            .pairing_expires()
            .filter(|_| self.core.pairing_open(now))
            .map(|e| (self.start + e.as_duration()).into_std());
        self.trust.set_pairing_until(until);
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
                        let flushed = e.writers.get(&ChannelClass::Control).is_some_and(|w| {
                            w.send(WriterMsg::CloseAfter {
                                conn: e.conn.clone(),
                                code,
                                reason: reason.clone(),
                            })
                            .is_ok()
                        });
                        let c = e.conn.clone();
                        if flushed {
                            // Fallback in case the control stream is stuck.
                            tokio::spawn(async move {
                                tokio::time::sleep(CLOSE_FLUSH_LIMIT * 2).await;
                                c.close(code, &reason);
                            });
                        } else {
                            c.close(code, &reason);
                        }
                    }
                }
                Action::Event(event) => {
                    peers_changed |= matches!(
                        event,
                        CoreEvent::PeerUp(_) | CoreEvent::PeerDown { .. } | CoreEvent::Clock { .. }
                    );
                    self.on_event(&event);
                    let _ = self.events.send(NodeEvent::Core(event));
                }
            }
        }
        if peers_changed {
            self.refresh_status();
        }
    }

    fn on_event(&self, event: &CoreEvent) {
        match event {
            CoreEvent::Paired { peer, role, .. } => {
                info!(%peer, ?role, "paired");
                if self.trust.get(peer).is_none() {
                    self.trust.insert(TrustedPeer::own_device(*peer));
                }
            }
            CoreEvent::PeerUp(p) => {
                let addr = self
                    .conns
                    .values()
                    .find(|c| c.conn.peer() == p.id)
                    .map(|c| c.conn.remote())
                    .map(|PeerAddr::Ip(a)| a);
                self.trust.note_seen(&p.id, &p.name, addr);
            }
            CoreEvent::Unpaired { peer } => {
                info!(%peer, "the peer revoked our pairing");
                self.trust.remove(peer);
            }
            CoreEvent::LayoutChanged(l) => {
                self.status.lock().unwrap_or_else(|p| p.into_inner()).layout = Some(l.clone());
            }
            CoreEvent::Rejected { code, message, .. } => {
                warn!(?code, "connection rejected: {message}")
            }
            CoreEvent::PairingFailed { peer, reason, .. } => {
                warn!(%peer, "pairing failed: {reason}")
            }
            _ => {}
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
        let _ = writer.send(WriterMsg::Frame(frame));
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
