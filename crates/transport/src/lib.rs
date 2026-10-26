//! `Transport`/`Connection` abstractions and their QUIC implementation (ADR-0002).

mod api;
mod error;
pub mod future;
mod identity;
mod quic;
mod verify;

pub use api::{
    BoxFuture, Connection, PathStats, PeerAddr, RecvStream, SendStream, StreamPair, Transport,
    TransportCaps,
};
pub use error::{CloseInfo, ClosedBy, TransportError};
pub use identity::{DeviceIdentity, device_id_from_cert, device_id_from_spki};
pub use quic::{ALPN_BULK, ALPN_INTERACTIVE, QuicConnection, QuicOptions, QuicTransport};
pub use quinn::{AsyncUdpSocket, UdpPoller, udp};
pub use verify::{ExpectedPeer, TrustPolicy};
