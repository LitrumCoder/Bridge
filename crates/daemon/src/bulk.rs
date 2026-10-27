//! The bulk connections of a node (ADR-0002): dialed lazily by the side that sends
//! files, with the delay-based controller; accepted ones only deliver incoming streams.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use bridge_core::ConnectionKind;
use bridge_protocol::{ChannelClass, DeviceId};
use bridge_transport::{
    BoxFuture, BulkLink, Connection, ExpectedPeer, PeerAddr, QuicTransport, RecvStream, SendStream,
    Transport, TransportError,
};
use tokio::sync::mpsc;
use tracing::debug;

type Incoming = mpsc::UnboundedSender<(DeviceId, Box<dyn RecvStream>)>;

#[derive(Default)]
pub(crate) struct BulkHub {
    transport: OnceLock<Arc<QuicTransport>>,
    /// Where each peer's interactive connection came from: the bulk dial target.
    addrs: Mutex<HashMap<DeviceId, SocketAddr>>,
    dialed: tokio::sync::Mutex<HashMap<DeviceId, Arc<dyn Connection>>>,
    incoming: Mutex<Option<Incoming>>,
}

impl BulkHub {
    pub(crate) fn set_transport(&self, t: Arc<QuicTransport>) {
        let _ = self.transport.set(t);
    }

    pub(crate) fn set_incoming(&self, tx: Incoming) {
        *self.incoming.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
    }

    pub(crate) fn note_peer(&self, peer: DeviceId, addr: SocketAddr) {
        self.addrs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(peer, addr);
    }

    /// Serves an accepted bulk connection: every uni stream goes to the files feature.
    pub(crate) async fn serve(self: Arc<Self>, conn: Arc<dyn Connection>) {
        let peer = conn.peer();
        while let Ok(mut stream) = conn.accept_uni().await {
            let mut tag = [0u8; 1];
            let ok = matches!(stream.read(&mut tag).await, Ok(Some(1)))
                && tag[0] == ChannelClass::Bulk.stream_tag();
            let tx = self
                .incoming
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            match tx {
                Some(tx) if ok => {
                    let _ = tx.send((peer, stream));
                }
                _ => stream.stop(1),
            }
        }
        debug!(%peer, "incoming bulk connection closed");
    }

    async fn connection(&self, peer: DeviceId) -> Result<Arc<dyn Connection>, TransportError> {
        let mut dialed = self.dialed.lock().await;
        if let Some(c) = dialed.get(&peer) {
            return Ok(c.clone());
        }
        let transport = self
            .transport
            .get()
            .ok_or_else(|| TransportError::Other("node not started".into()))?;
        let addr = self
            .addrs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&peer)
            .copied()
            .ok_or_else(|| TransportError::Other("peer is not connected".into()))?;
        let conn = transport
            .connect(
                PeerAddr::Ip(addr),
                ExpectedPeer::Device(peer),
                ConnectionKind::Bulk,
            )
            .await?;
        dialed.insert(peer, conn.clone());
        Ok(conn)
    }

    async fn open_once(&self, peer: DeviceId) -> Result<Box<dyn SendStream>, TransportError> {
        let conn = self.connection(peer).await?;
        let mut s = match conn.open_uni(0).await {
            Ok(s) => s,
            Err(e) => {
                self.dialed.lock().await.remove(&peer);
                return Err(e);
            }
        };
        s.write_all(&[ChannelClass::Bulk.stream_tag()]).await?;
        Ok(s)
    }
}

impl BulkLink for BulkHub {
    fn open(&self, peer: DeviceId) -> BoxFuture<'_, Result<Box<dyn SendStream>, TransportError>> {
        Box::pin(async move {
            match self.open_once(peer).await {
                Ok(s) => Ok(s),
                // The old connection may have died (idle, network change): dial again once.
                Err(_) => self.open_once(peer).await,
            }
        })
    }
}
