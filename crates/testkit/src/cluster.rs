//! Several `bridge_node`s in one process on a simulated network.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bridge_daemon::{NodeBuilder, NodeHandle, TrustStore, TrustedPeer};
use bridge_protocol::DeviceId;
use bridge_transport::{DeviceIdentity, QuicOptions, QuicTransport, TransportError};

use crate::Network;

pub struct TestNode {
    pub handle: NodeHandle,
    pub addr: SocketAddr,
    pub id: DeviceId,
}

pub struct NodeSpec {
    pub identity: DeviceIdentity,
    pub trust: Arc<TrustStore>,
    pub socket: Arc<crate::SimSocket>,
}

/// Prepares `n` identities and sockets; every node trusts every other one.
pub fn mutually_trusting(net: &Network, n: usize) -> Result<Vec<NodeSpec>, TransportError> {
    let identities: Vec<DeviceIdentity> = (0..n)
        .map(|_| DeviceIdentity::generate())
        .collect::<Result<_, _>>()?;
    let ids: Vec<DeviceId> = identities.iter().map(DeviceIdentity::id).collect();
    Ok(identities
        .into_iter()
        .map(|identity| {
            let me = identity.id();
            let trust = TrustStore::new(
                ids.iter()
                    .filter(|i| **i != me)
                    .map(|i| TrustedPeer::own_device(*i)),
            );
            NodeSpec {
                identity,
                trust,
                socket: net.socket(),
            }
        })
        .collect())
}

/// Prepares `n` nodes that trust nobody (pairing tests).
pub fn strangers(net: &Network, n: usize) -> Result<Vec<NodeSpec>, TransportError> {
    (0..n)
        .map(|_| {
            Ok(NodeSpec {
                identity: DeviceIdentity::generate()?,
                trust: TrustStore::new([]),
                socket: net.socket(),
            })
        })
        .collect()
}

/// Fast failure detection so tests about loss and partitions stay short.
pub fn test_options() -> QuicOptions {
    QuicOptions {
        keep_alive: Duration::from_millis(100),
        idle_timeout: Duration::from_millis(800),
    }
}

impl NodeSpec {
    pub fn start(self, name: &str, configure: impl FnOnce(NodeBuilder) -> NodeBuilder) -> TestNode {
        let addr = self.socket.addr();
        let id = self.identity.id();
        let transport = QuicTransport::with_socket(
            self.socket,
            self.identity,
            self.trust.clone(),
            test_options(),
        )
        .unwrap_or_else(|e| panic!("transport for {name}: {e}"));
        let handle = configure(NodeBuilder::new(name, self.trust))
            .start(transport)
            .unwrap_or_else(|e| panic!("node {name}: {e}"));
        TestNode { handle, addr, id }
    }
}
