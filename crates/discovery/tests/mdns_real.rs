//! Real multicast DNS on this machine. Multicast is often blocked in CI containers,
//! so the test only runs with `BRIDGE_MDNS_TEST=1`.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use bridge_discovery::*;
use bridge_protocol::DeviceId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_responders_find_each_other() {
    if std::env::var_os("BRIDGE_MDNS_TEST").is_none() {
        eprintln!("skipped: set BRIDGE_MDNS_TEST=1 to run against real multicast");
        return;
    }
    let (a, b) = (DeviceId([0xA1; 32]), DeviceId([0xB2; 32]));
    let da = MdnsDiscovery::start(a).unwrap();
    let db = MdnsDiscovery::start(b).unwrap();
    let mut events = da.subscribe();
    db.advertise(&Advertisement {
        id: b,
        name: Some("mdns-test-b".into()),
        port: 47999,
        state: AdState::Connected,
    })
    .unwrap();
    da.advertise(&Advertisement {
        id: a,
        name: None,
        port: 47998,
        state: AdState::Connected,
    })
    .unwrap();
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(DiscoveryEvent::Found(d)) = events.recv().await
                && d.id == b
            {
                return d;
            }
        }
    })
    .await
    .expect("device b announced via mDNS");
    assert_eq!(found.name.as_deref(), Some("mdns-test-b"));
    assert!(found.addrs.iter().all(|a| a.port() == 47999));
    assert!(!found.addrs.is_empty());

    db.withdraw();
    let lost = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(DiscoveryEvent::Lost(id)) = events.recv().await {
                return id;
            }
        }
    })
    .await;
    assert_eq!(lost.ok(), Some(b));
}
