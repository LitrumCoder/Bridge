//! mDNS / DNS-SD implementation on `mdns-sd` (pure Rust, Apache-2.0 OR MIT).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bridge_protocol::DeviceId;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    Advertisement, Discovery, DiscoveryError, DiscoveryEvent, SERVICE_TYPE, instance_name,
    parse_txt, txt_properties,
};

type Subscribers = Arc<Mutex<Vec<mpsc::UnboundedSender<DiscoveryEvent>>>>;

pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    own: Mutex<Option<String>>,
    subscribers: Subscribers,
}

impl MdnsDiscovery {
    /// Starts the mDNS responder and browser. Must run inside a tokio runtime.
    pub fn start(own_id: DeviceId) -> Result<Arc<Self>, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Mdns(e.to_string()))?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;
        let subscribers: Subscribers = Arc::default();
        let subs = subscribers.clone();
        tokio::spawn(async move {
            // fullname → device, to translate removals.
            let mut known: HashMap<String, DeviceId> = HashMap::new();
            while let Ok(event) = events.recv_async().await {
                let mapped = match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let addrs: Vec<SocketAddr> = info
                            .addresses
                            .iter()
                            .map(|a| SocketAddr::new(a.to_ip_addr(), info.port))
                            .collect();
                        let found =
                            parse_txt(|k| info.get_property_val_str(k).map(str::to_owned), addrs);
                        match found {
                            Some(d) if d.id != own_id => {
                                known.insert(info.fullname.clone(), d.id);
                                Some(DiscoveryEvent::Found(d))
                            }
                            _ => None,
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        known.remove(&fullname).map(DiscoveryEvent::Lost)
                    }
                    other => {
                        debug!(?other, "mdns event");
                        None
                    }
                };
                if let Some(ev) = mapped {
                    let mut subs = subs.lock().unwrap_or_else(|p| p.into_inner());
                    subs.retain(|s| s.send(ev.clone()).is_ok());
                }
            }
        });
        Ok(Arc::new(Self {
            daemon,
            own: Mutex::new(None),
            subscribers,
        }))
    }
}

impl Discovery for MdnsDiscovery {
    fn advertise(&self, ad: &Advertisement) -> Result<(), DiscoveryError> {
        let instance = instance_name(&ad.id);
        let host = format!("{instance}.local.");
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host,
            "",
            ad.port,
            txt_properties(ad),
        )
        .map_err(|e| DiscoveryError::Mdns(e.to_string()))?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon
            .register(info)
            .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;
        *self.own.lock().unwrap_or_else(|p| p.into_inner()) = Some(fullname);
        Ok(())
    }

    fn withdraw(&self) {
        if let Some(fullname) = self.own.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = self.daemon.unregister(&fullname);
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<DiscoveryEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(tx);
        rx
    }
}

impl Drop for MdnsDiscovery {
    fn drop(&mut self) {
        self.withdraw();
        let _ = self.daemon.shutdown();
    }
}
