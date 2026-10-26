//! In-process discovery bus for tests and the network simulator.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bridge_protocol::DeviceId;
use tokio::sync::mpsc;

use crate::{Advertisement, Discovered, Discovery, DiscoveryError, DiscoveryEvent};

#[derive(Default)]
struct Bus {
    ads: HashMap<DeviceId, Discovered>,
    subscribers: Vec<(DeviceId, mpsc::UnboundedSender<DiscoveryEvent>)>,
}

#[derive(Clone, Default)]
pub struct MemoryBus {
    bus: Arc<Mutex<Bus>>,
}

impl MemoryBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// An endpoint for a device reachable at `addr`.
    pub fn endpoint(&self, id: DeviceId, addr: SocketAddr) -> Arc<MemoryDiscovery> {
        Arc::new(MemoryDiscovery {
            bus: self.clone(),
            id,
            addr,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bus> {
        self.bus.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn publish(&self, from: DeviceId, ev: DiscoveryEvent) {
        let mut bus = self.lock();
        bus.subscribers
            .retain(|(id, tx)| *id == from || tx.send(ev.clone()).is_ok());
    }
}

pub struct MemoryDiscovery {
    bus: MemoryBus,
    id: DeviceId,
    addr: SocketAddr,
}

impl Discovery for MemoryDiscovery {
    fn advertise(&self, ad: &Advertisement) -> Result<(), DiscoveryError> {
        let d = Discovered {
            id: ad.id,
            name: ad.name.clone(),
            addrs: vec![self.addr],
            state: ad.state,
        };
        self.bus.lock().ads.insert(self.id, d.clone());
        self.bus.publish(self.id, DiscoveryEvent::Found(d));
        Ok(())
    }

    fn withdraw(&self) {
        if self.bus.lock().ads.remove(&self.id).is_some() {
            self.bus.publish(self.id, DiscoveryEvent::Lost(self.id));
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<DiscoveryEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bus = self.bus.lock();
        for d in bus.ads.values().filter(|d| d.id != self.id) {
            let _ = tx.send(DiscoveryEvent::Found(d.clone()));
        }
        bus.subscribers.push((self.id, tx));
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdState;

    #[tokio::test]
    async fn endpoints_see_each_other_but_not_themselves() {
        let bus = MemoryBus::new();
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let ea = bus.endpoint(a, "10.0.0.1:1".parse().unwrap());
        let eb = bus.endpoint(b, "10.0.0.2:2".parse().unwrap());
        let ad = |id| Advertisement {
            id,
            name: Some("x".into()),
            port: 1,
            state: AdState::Connected,
        };
        ea.advertise(&ad(a)).unwrap();
        let mut rb = eb.subscribe();
        let mut ra = ea.subscribe();
        eb.advertise(&ad(b)).unwrap();
        assert!(matches!(rb.recv().await, Some(DiscoveryEvent::Found(d)) if d.id == a));
        assert!(matches!(ra.recv().await, Some(DiscoveryEvent::Found(d)) if d.id == b));
        eb.withdraw();
        assert_eq!(ra.recv().await, Some(DiscoveryEvent::Lost(b)));
        assert!(rb.try_recv().is_err(), "own events are not delivered");
    }
}
