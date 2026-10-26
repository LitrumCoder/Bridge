//! Transport-neutral connection API used by the node driver (ARCHITECTURE §6).
//! QUIC is the only implementation in stage 1; BLE, Wi-Fi Direct, cable and relay
//! transports plug in behind the same traits (see `future`).

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bridge_core::ConnectionKind;
use bridge_protocol::{CloseCode, DeviceId};
use bytes::Bytes;

use crate::{CloseInfo, ExpectedPeer, TransportError};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type StreamPair = (Box<dyn SendStream>, Box<dyn RecvStream>);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PeerAddr {
    Ip(SocketAddr),
    // TODO(roadmap): Ble(BleAddr), WifiDirect(..), Relay(..)
}

impl fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(a) => a.fmt(f),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransportCaps {
    pub datagrams: bool,
    pub multiplexed_streams: bool,
    /// Rough class for the channel scheduler: bytes per second.
    pub typical_bandwidth: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PathStats {
    pub rtt: Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_window: u64,
}

pub trait Transport: Send + Sync + 'static {
    fn caps(&self) -> TransportCaps;

    fn local_addr(&self) -> Result<PeerAddr, TransportError>;

    fn connect(
        &self,
        addr: PeerAddr,
        expect: ExpectedPeer,
        kind: ConnectionKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Connection>, TransportError>>;

    /// Next authenticated incoming connection; `None` once the transport is closed.
    fn accept(&self) -> BoxFuture<'_, Option<Result<Arc<dyn Connection>, TransportError>>>;

    fn close(&self);
}

pub trait Connection: Send + Sync + 'static {
    /// Identity proven by the peer's certificate.
    fn peer(&self) -> DeviceId;
    fn kind(&self) -> ConnectionKind;
    fn remote(&self) -> PeerAddr;

    fn open_uni(&self, priority: i32)
    -> BoxFuture<'_, Result<Box<dyn SendStream>, TransportError>>;
    fn accept_uni(&self) -> BoxFuture<'_, Result<Box<dyn RecvStream>, TransportError>>;
    fn open_bi(&self, priority: i32) -> BoxFuture<'_, Result<StreamPair, TransportError>>;
    fn accept_bi(&self) -> BoxFuture<'_, Result<StreamPair, TransportError>>;

    fn send_datagram(&self, data: Bytes) -> Result<(), TransportError>;
    fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>>;
    fn max_datagram_size(&self) -> Option<usize>;

    fn stats(&self) -> PathStats;
    fn close(&self, code: CloseCode, reason: &str);
    /// Resolves when the connection is gone, with who closed it and why.
    fn closed(&self) -> BoxFuture<'_, CloseInfo>;
}

pub trait SendStream: Send + 'static {
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<(), TransportError>>;
    fn finish(&mut self) -> Result<(), TransportError>;
    fn reset(&mut self, code: u32);
}

pub trait RecvStream: Send + 'static {
    /// `Ok(None)` at the end of the stream.
    fn read<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, Result<Option<usize>, TransportError>>;
    fn stop(&mut self, code: u32);
}
