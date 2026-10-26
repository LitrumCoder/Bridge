//! Real QUIC over the simulated network.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bridge_core::ConnectionKind;
use bridge_protocol::DeviceId;
use bridge_testkit::{LinkConfig, Network};
use bridge_transport::*;

#[derive(Default)]
struct Trust(Mutex<HashSet<DeviceId>>);

impl TrustPolicy for Trust {
    fn is_trusted(&self, id: &DeviceId) -> bool {
        self.0.lock().unwrap().contains(id)
    }
}

struct Pair {
    a: Arc<QuicTransport>,
    b: Arc<QuicTransport>,
    a_addr: std::net::SocketAddr,
    b_addr: std::net::SocketAddr,
    id_b: DeviceId,
}

fn pair(net: &Network, options: QuicOptions) -> Pair {
    let (ia, ib) = (
        DeviceIdentity::generate().unwrap(),
        DeviceIdentity::generate().unwrap(),
    );
    let trust = Arc::new(Trust::default());
    trust.0.lock().unwrap().extend([ia.id(), ib.id()]);
    let (sa, sb) = (net.socket(), net.socket());
    let (a_addr, b_addr) = (sa.addr(), sb.addr());
    let id_b = ib.id();
    let a = QuicTransport::with_socket(sa, ia, trust.clone(), options.clone()).unwrap();
    let b = QuicTransport::with_socket(sb, ib, trust, options).unwrap();
    Pair {
        a: Arc::new(a),
        b: Arc::new(b),
        a_addr,
        b_addr,
        id_b,
    }
}

async fn read_all(rx: &mut Box<dyn RecvStream>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while let Some(n) = rx.read(&mut buf).await.unwrap() {
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_survives_loss_jitter_and_reordering() {
    let net = Network::new(7);
    net.set_default_link(LinkConfig {
        latency: Duration::from_millis(5),
        jitter: Duration::from_millis(4),
        loss: 0.05,
        ..LinkConfig::default()
    });
    let p = pair(&net, QuicOptions::default());
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i * 31 % 251) as u8).collect();

    let b = p.b.clone();
    let server = tokio::spawn(async move {
        let conn = b.accept().await.unwrap().unwrap();
        let mut rx = conn.accept_uni().await.unwrap();
        let got = read_all(&mut rx).await;
        (got, conn)
    });
    let conn =
        p.a.connect(
            PeerAddr::Ip(p.b_addr),
            ExpectedPeer::Device(p.id_b),
            ConnectionKind::Interactive,
        )
        .await
        .unwrap();
    let mut tx = conn.open_uni(10).await.unwrap();
    tx.write_all(&payload).await.unwrap();
    tx.finish().unwrap();
    let (got, _server_conn) = server.await.unwrap();
    assert_eq!(got.len(), payload.len());
    assert!(got == payload, "bytes must arrive intact and in order");
    let stats = net.stats();
    assert!(stats.lost > 0, "the link really dropped packets: {stats:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_is_detected_by_idle_timeout() {
    let net = Network::new(1);
    let options = QuicOptions {
        keep_alive: Duration::from_millis(100),
        idle_timeout: Duration::from_millis(600),
    };
    let p = pair(&net, options);
    let b = p.b.clone();
    let server = tokio::spawn(async move { b.accept().await.unwrap().unwrap() });
    let conn =
        p.a.connect(
            PeerAddr::Ip(p.b_addr),
            ExpectedPeer::Device(p.id_b),
            ConnectionKind::Interactive,
        )
        .await
        .unwrap();
    let server_conn = server.await.unwrap();

    let started = Instant::now();
    net.partition(p.a_addr, p.b_addr);
    let (ca, cb) = tokio::join!(conn.closed(), server_conn.closed());
    assert_eq!(ca.by, ClosedBy::Transport);
    assert_eq!(cb.by, ClosedBy::Transport);
    assert_eq!(ca.code, None);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    assert!(net.stats().partitioned > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bottleneck_limits_throughput() {
    let net = Network::new(3);
    net.set_default_link(LinkConfig {
        latency: Duration::from_millis(2),
        bandwidth_bps: Some(40_000_000),
        queue_bytes: 64 * 1024,
        ..LinkConfig::default()
    });
    let p = pair(&net, QuicOptions::default());
    let b = p.b.clone();
    let server = tokio::spawn(async move {
        let conn = b.accept().await.unwrap().unwrap();
        let mut rx = conn.accept_uni().await.unwrap();
        let n = read_all(&mut rx).await.len();
        (n, conn)
    });
    let conn =
        p.a.connect(
            PeerAddr::Ip(p.b_addr),
            ExpectedPeer::Device(p.id_b),
            ConnectionKind::Bulk,
        )
        .await
        .unwrap();
    let data = vec![0u8; 2_000_000];
    let started = Instant::now();
    let mut tx = conn.open_uni(0).await.unwrap();
    tx.write_all(&data).await.unwrap();
    tx.finish().unwrap();
    let (n, _c) = server.await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(n, data.len());
    let mbps = n as f64 * 8.0 / secs / 1e6;
    // 2 MB over 40 Mbit/s needs at least 0.4 s; allow slow CI machines on the upper side.
    assert!(mbps <= 41.0, "faster than the bottleneck: {mbps:.1} Mbit/s");
    assert!(mbps > 5.0, "unexpectedly slow: {mbps:.1} Mbit/s");
}
