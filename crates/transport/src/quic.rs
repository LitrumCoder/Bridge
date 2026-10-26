//! QUIC implementation of [`Transport`] on quinn + rustls (ring), ADR-0002.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::debug;

use bridge_core::ConnectionKind;
use bridge_protocol::{CloseCode, DeviceId};
use bytes::Bytes;
use quinn::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use quinn::{
    AsyncUdpSocket, ConnectionError, Endpoint, EndpointConfig, IdleTimeout, TokioRuntime, VarInt,
};
use rustls::crypto::CryptoProvider;
use rustls_pki_types::CertificateDer;

use crate::api::{
    BoxFuture, Connection, PathStats, PeerAddr, RecvStream, SendStream, StreamPair, Transport,
    TransportCaps,
};
use crate::identity::device_id_from_cert;
use crate::verify::{PinnedClientVerifier, PinnedServerVerifier};
use crate::{CloseInfo, ClosedBy, DeviceIdentity, ExpectedPeer, TransportError, TrustPolicy};

pub const ALPN_INTERACTIVE: &[u8] = b"bridge";
pub const ALPN_BULK: &[u8] = b"bridge-bulk";

/// TLS alert `access_denied` (49) carried as a QUIC crypto error (0x100 + alert).
const CRYPTO_ACCESS_DENIED: u64 = 0x100 + 49;
const SERVER_NAME: &str = "bridge";

#[derive(Clone, Debug)]
pub struct QuicOptions {
    pub keep_alive: Duration,
    /// Short on purpose: a lost peer must release pressed keys quickly (ADR-0005).
    pub idle_timeout: Duration,
}

impl Default for QuicOptions {
    fn default() -> Self {
        Self {
            keep_alive: Duration::from_millis(500),
            idle_timeout: Duration::from_secs(3),
        }
    }
}

pub struct QuicTransport {
    endpoint: Endpoint,
    identity: DeviceIdentity,
    provider: Arc<CryptoProvider>,
    options: QuicOptions,
    accepted: tokio::sync::Mutex<mpsc::Receiver<Result<QuicConnection, TransportError>>>,
}

/// A host may run at most this many handshakes with us at once (threat model §10.3).
const MAX_HANDSHAKES_PER_IP: usize = 4;
const MAX_HANDSHAKES: usize = 64;
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Inflight {
    per_ip: HashMap<IpAddr, usize>,
    total: usize,
}

struct Slot {
    inflight: Arc<Mutex<Inflight>>,
    ip: IpAddr,
}

impl Slot {
    fn take(inflight: &Arc<Mutex<Inflight>>, ip: IpAddr) -> Option<Self> {
        let mut f = inflight.lock().unwrap_or_else(|p| p.into_inner());
        let n = f.per_ip.get(&ip).copied().unwrap_or(0);
        if n >= MAX_HANDSHAKES_PER_IP || f.total >= MAX_HANDSHAKES {
            return None;
        }
        f.per_ip.insert(ip, n + 1);
        f.total += 1;
        Some(Self {
            inflight: inflight.clone(),
            ip,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut f = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        f.total = f.total.saturating_sub(1);
        if let Some(n) = f.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                f.per_ip.remove(&self.ip);
            }
        }
    }
}

/// Accepts incoming connections and runs their handshakes in parallel, so one slow or
/// malicious client cannot stall the others.
async fn acceptor(endpoint: Endpoint, tx: mpsc::Sender<Result<QuicConnection, TransportError>>) {
    let inflight = Arc::new(Mutex::new(Inflight::default()));
    while let Some(incoming) = endpoint.accept().await {
        if !incoming.remote_address_validated() {
            // Address validation before any expensive work (no amplification).
            let _ = incoming.retry();
            continue;
        }
        let Some(slot) = Slot::take(&inflight, incoming.remote_address().ip()) else {
            debug!(addr = %incoming.remote_address(), "too many handshakes, refusing");
            incoming.refuse();
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(HANDSHAKE_LIMIT, incoming).await {
                Err(_) => Err(TransportError::TimedOut),
                Ok(Err(e)) => Err(map_connect_error(e)),
                Ok(Ok(conn)) => match (peer_id(&conn), alpn_kind(&conn)) {
                    (Some(peer), Some(kind)) => Ok(QuicConnection::new(conn, peer, kind)),
                    _ => {
                        conn.close(
                            VarInt::from_u32(CloseCode::PROTOCOL_VIOLATION.0),
                            b"bad handshake",
                        );
                        Err(TransportError::BadIdentity("peer certificate or alpn"))
                    }
                },
            };
            drop(slot);
            let _ = tx.send(result).await;
        });
    }
}

impl QuicTransport {
    fn from_endpoint(
        endpoint: Endpoint,
        identity: DeviceIdentity,
        provider: Arc<CryptoProvider>,
        options: QuicOptions,
    ) -> Self {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(acceptor(endpoint.clone(), tx));
        Self {
            endpoint,
            identity,
            provider,
            options,
            accepted: tokio::sync::Mutex::new(rx),
        }
    }

    /// Binds a UDP socket on `addr` (port 0 picks a free port).
    pub fn bind(
        addr: SocketAddr,
        identity: DeviceIdentity,
        trust: Arc<dyn TrustPolicy>,
        options: QuicOptions,
    ) -> Result<Self, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = server_config(&identity, trust, &provider, &options)?;
        let socket = std::net::UdpSocket::bind(addr)?;
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(server),
            socket,
            Arc::new(TokioRuntime),
        )?;
        Ok(Self::from_endpoint(endpoint, identity, provider, options))
    }

    /// Uses a custom socket, e.g. the in-memory network simulator of the testkit.
    pub fn with_socket(
        socket: Arc<dyn AsyncUdpSocket>,
        identity: DeviceIdentity,
        trust: Arc<dyn TrustPolicy>,
        options: QuicOptions,
    ) -> Result<Self, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = server_config(&identity, trust, &provider, &options)?;
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server),
            socket,
            Arc::new(TokioRuntime),
        )?;
        Ok(Self::from_endpoint(endpoint, identity, provider, options))
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub async fn connect_quic(
        &self,
        addr: SocketAddr,
        expect: ExpectedPeer,
        kind: ConnectionKind,
    ) -> Result<QuicConnection, TransportError> {
        let client = client_config(&self.identity, expect, kind, &self.provider, &self.options)?;
        let connecting = self
            .endpoint
            .connect_with(client, addr, SERVER_NAME)
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let conn = connecting.await.map_err(map_connect_error)?;
        let peer = peer_id(&conn).ok_or(TransportError::BadIdentity("peer certificate"))?;
        if let ExpectedPeer::Device(want) = expect
            && want != peer
        {
            conn.close(
                VarInt::from_u32(CloseCode::PROTOCOL_VIOLATION.0),
                b"unexpected identity",
            );
            return Err(TransportError::WrongPeer);
        }
        Ok(QuicConnection::new(conn, peer, kind))
    }

    pub async fn accept_quic(&self) -> Option<Result<QuicConnection, TransportError>> {
        self.accepted.lock().await.recv().await
    }
}

impl Transport for QuicTransport {
    fn caps(&self) -> TransportCaps {
        TransportCaps {
            datagrams: true,
            multiplexed_streams: true,
            typical_bandwidth: 50_000_000,
        }
    }

    fn local_addr(&self) -> Result<PeerAddr, TransportError> {
        Ok(PeerAddr::Ip(self.endpoint.local_addr()?))
    }

    fn connect(
        &self,
        addr: PeerAddr,
        expect: ExpectedPeer,
        kind: ConnectionKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Connection>, TransportError>> {
        Box::pin(async move {
            let PeerAddr::Ip(addr) = addr;
            let conn = self.connect_quic(addr, expect, kind).await?;
            Ok(Arc::new(conn) as Arc<dyn Connection>)
        })
    }

    fn accept(&self) -> BoxFuture<'_, Option<Result<Arc<dyn Connection>, TransportError>>> {
        Box::pin(async move {
            self.accept_quic()
                .await
                .map(|r| r.map(|c| Arc::new(c) as Arc<dyn Connection>))
        })
    }

    fn close(&self) {
        self.endpoint
            .close(VarInt::from_u32(CloseCode::NORMAL.0), b"shutdown");
    }
}

fn transport_config(options: &QuicOptions) -> Result<Arc<quinn::TransportConfig>, TransportError> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(options.keep_alive));
    let idle = IdleTimeout::try_from(options.idle_timeout)
        .map_err(|_| TransportError::Other("idle timeout out of range".into()))?;
    t.max_idle_timeout(Some(idle));
    // TODO(M6a): delay-based congestion controller for bulk connections (ADR-0002).
    Ok(Arc::new(t))
}

fn server_config(
    identity: &DeviceIdentity,
    trust: Arc<dyn TrustPolicy>,
    provider: &Arc<CryptoProvider>,
    options: &QuicOptions,
) -> Result<quinn::ServerConfig, TransportError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_client_cert_verifier(PinnedClientVerifier::new(trust, provider))
        .with_single_cert(vec![identity.cert()], identity.key())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    tls.alpn_protocols = vec![ALPN_INTERACTIVE.to_vec(), ALPN_BULK.to_vec()];
    let quic = QuicServerConfig::try_from(tls).map_err(|e| TransportError::Tls(e.to_string()))?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic));
    server.transport_config(transport_config(options)?);
    Ok(server)
}

fn client_config(
    identity: &DeviceIdentity,
    expect: ExpectedPeer,
    kind: ConnectionKind,
    provider: &Arc<CryptoProvider>,
    options: &QuicOptions,
) -> Result<quinn::ClientConfig, TransportError> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(PinnedServerVerifier::new(expect, provider))
        .with_client_auth_cert(vec![identity.cert()], identity.key())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    tls.alpn_protocols = vec![match kind {
        ConnectionKind::Interactive => ALPN_INTERACTIVE.to_vec(),
        ConnectionKind::Bulk => ALPN_BULK.to_vec(),
    }];
    let quic = QuicClientConfig::try_from(tls).map_err(|e| TransportError::Tls(e.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(quic));
    client.transport_config(transport_config(options)?);
    Ok(client)
}

fn peer_id(conn: &quinn::Connection) -> Option<DeviceId> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    device_id_from_cert(certs.first()?)
}

fn alpn_kind(conn: &quinn::Connection) -> Option<ConnectionKind> {
    let data = conn.handshake_data()?.downcast::<HandshakeData>().ok()?;
    match data.protocol.as_deref()? {
        ALPN_INTERACTIVE => Some(ConnectionKind::Interactive),
        ALPN_BULK => Some(ConnectionKind::Bulk),
        _ => None,
    }
}

fn map_connect_error(e: ConnectionError) -> TransportError {
    match e {
        ConnectionError::ConnectionClosed(c) if u64::from(c.error_code) == CRYPTO_ACCESS_DENIED => {
            TransportError::NotPaired
        }
        ConnectionError::TransportError(t) if u64::from(t.code) == CRYPTO_ACCESS_DENIED => {
            TransportError::WrongPeer
        }
        ConnectionError::TimedOut => TransportError::TimedOut,
        other => TransportError::Closed(close_info(other, None)),
    }
}

fn close_info(e: ConnectionError, local: Option<&(CloseCode, String)>) -> CloseInfo {
    match e {
        ConnectionError::ApplicationClosed(a) => CloseInfo {
            by: ClosedBy::Peer,
            code: u32::try_from(a.error_code.into_inner()).ok().map(CloseCode),
            reason: String::from_utf8_lossy(&a.reason).into_owned(),
        },
        // In TLS 1.3 the client may finish its side of the handshake before the server
        // has judged the client certificate, so a refusal arrives as a close.
        ConnectionError::ConnectionClosed(c) if u64::from(c.error_code) == CRYPTO_ACCESS_DENIED => {
            CloseInfo {
                by: ClosedBy::Peer,
                code: Some(CloseCode::NOT_PAIRED),
                reason: "the peer does not trust this device".to_owned(),
            }
        }
        ConnectionError::LocallyClosed => CloseInfo {
            by: ClosedBy::Local,
            code: local.map(|(c, _)| *c),
            reason: local.map(|(_, r)| r.clone()).unwrap_or_default(),
        },
        other => CloseInfo {
            by: ClosedBy::Transport,
            code: None,
            reason: other.to_string(),
        },
    }
}

pub struct QuicConnection {
    conn: quinn::Connection,
    peer: DeviceId,
    kind: ConnectionKind,
    local_close: OnceLock<(CloseCode, String)>,
}

impl QuicConnection {
    fn new(conn: quinn::Connection, peer: DeviceId, kind: ConnectionKind) -> Self {
        Self {
            conn,
            peer,
            kind,
            local_close: OnceLock::new(),
        }
    }

    fn lost(&self, e: ConnectionError) -> TransportError {
        TransportError::Closed(close_info(e, self.local_close.get()))
    }
}

impl Connection for QuicConnection {
    fn peer(&self) -> DeviceId {
        self.peer
    }

    fn kind(&self) -> ConnectionKind {
        self.kind
    }

    fn remote(&self) -> PeerAddr {
        PeerAddr::Ip(self.conn.remote_address())
    }

    fn open_uni(
        &self,
        priority: i32,
    ) -> BoxFuture<'_, Result<Box<dyn SendStream>, TransportError>> {
        Box::pin(async move {
            let s = self.conn.open_uni().await.map_err(|e| self.lost(e))?;
            let _ = s.set_priority(priority);
            Ok(Box::new(QuicSend(s)) as Box<dyn SendStream>)
        })
    }

    fn accept_uni(&self) -> BoxFuture<'_, Result<Box<dyn RecvStream>, TransportError>> {
        Box::pin(async move {
            let s = self.conn.accept_uni().await.map_err(|e| self.lost(e))?;
            Ok(Box::new(QuicRecv(s)) as Box<dyn RecvStream>)
        })
    }

    fn open_bi(&self, priority: i32) -> BoxFuture<'_, Result<StreamPair, TransportError>> {
        Box::pin(async move {
            let (s, r) = self.conn.open_bi().await.map_err(|e| self.lost(e))?;
            let _ = s.set_priority(priority);
            Ok((
                Box::new(QuicSend(s)) as Box<dyn SendStream>,
                Box::new(QuicRecv(r)) as Box<dyn RecvStream>,
            ))
        })
    }

    fn accept_bi(&self) -> BoxFuture<'_, Result<StreamPair, TransportError>> {
        Box::pin(async move {
            let (s, r) = self.conn.accept_bi().await.map_err(|e| self.lost(e))?;
            Ok((
                Box::new(QuicSend(s)) as Box<dyn SendStream>,
                Box::new(QuicRecv(r)) as Box<dyn RecvStream>,
            ))
        })
    }

    fn send_datagram(&self, data: Bytes) -> Result<(), TransportError> {
        self.conn
            .send_datagram(data)
            .map_err(|e| TransportError::Other(e.to_string()))
    }

    fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>> {
        Box::pin(async move { self.conn.read_datagram().await.map_err(|e| self.lost(e)) })
    }

    fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    fn stats(&self) -> PathStats {
        let s = self.conn.stats();
        PathStats {
            rtt: self.conn.rtt(),
            lost_packets: s.path.lost_packets,
            sent_packets: s.path.sent_packets,
            congestion_window: s.path.cwnd,
        }
    }

    fn close(&self, code: CloseCode, reason: &str) {
        let _ = self.local_close.set((code, reason.to_owned()));
        self.conn.close(VarInt::from_u32(code.0), reason.as_bytes());
    }

    fn closed(&self) -> BoxFuture<'_, CloseInfo> {
        Box::pin(async move {
            let e = self.conn.closed().await;
            close_info(e, self.local_close.get())
        })
    }
}

struct QuicSend(quinn::SendStream);

impl SendStream for QuicSend {
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<(), TransportError>> {
        Box::pin(async move {
            self.0
                .write_all(buf)
                .await
                .map_err(|e| TransportError::Other(e.to_string()))
        })
    }

    fn finish(&mut self) -> Result<(), TransportError> {
        self.0
            .finish()
            .map_err(|e| TransportError::Other(e.to_string()))
    }

    fn reset(&mut self, code: u32) {
        let _ = self.0.reset(VarInt::from_u32(code));
    }
}

struct QuicRecv(quinn::RecvStream);

impl RecvStream for QuicRecv {
    fn read<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, Result<Option<usize>, TransportError>> {
        Box::pin(async move {
            self.0
                .read(buf)
                .await
                .map_err(|e| TransportError::Other(e.to_string()))
        })
    }

    fn stop(&mut self, code: u32) {
        let _ = self.0.stop(VarInt::from_u32(code));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_slots_are_limited_per_host_and_released() {
        let inflight = Arc::new(Mutex::new(Inflight::default()));
        let a: IpAddr = "10.0.0.1".parse().expect("ip");
        let b: IpAddr = "10.0.0.2".parse().expect("ip");
        let held: Vec<Slot> = (0..MAX_HANDSHAKES_PER_IP)
            .map(|_| Slot::take(&inflight, a).expect("slot"))
            .collect();
        assert!(
            Slot::take(&inflight, a).is_none(),
            "fifth handshake from one host is refused"
        );
        assert!(
            Slot::take(&inflight, b).is_some(),
            "other hosts are not affected"
        );
        drop(held);
        assert!(Slot::take(&inflight, a).is_some(), "slots are released");
        let f = inflight.lock().expect("lock");
        assert_eq!(f.total, 0);
        assert!(f.per_ip.is_empty());
    }
}
