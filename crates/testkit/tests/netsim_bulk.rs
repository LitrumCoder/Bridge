//! The bulk connection's delay-based controller keeps the bottleneck queue short, so
//! interactive traffic (input, MIDI) is not delayed by a file transfer (ADR-0002).
//!
//! The default run moves 24 MB; `BRIDGE_LONG_TESTS=1` moves the 2 GB of the milestone
//! criterion.

#![allow(clippy::unwrap_used, clippy::print_stderr)]

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

fn percentile(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() - 1) as f64 * p) as usize]
}

struct Run {
    base_p50: Duration,
    base_p99: Duration,
    load_p99: Duration,
    rate_bps: f64,
}

async fn run(kind: ConnectionKind, total: u64, seed: u64) -> Run {
    let net = Network::new(seed);
    let link = LinkConfig {
        latency: Duration::from_millis(5),
        bandwidth_bps: Some(40_000_000),
        queue_bytes: 1 << 20,
        ..LinkConfig::default()
    };
    let (ia, ib) = (
        DeviceIdentity::generate().unwrap(),
        DeviceIdentity::generate().unwrap(),
    );
    let trust = Arc::new(Trust::default());
    trust.0.lock().unwrap().extend([ia.id(), ib.id()]);
    let (sa, sb) = (net.socket(), net.socket());
    net.set_link_both(sa.addr(), sb.addr(), link);
    let b_addr = sb.addr();
    let id_b = ib.id();
    let opts = QuicOptions {
        keep_alive: Duration::from_millis(200),
        idle_timeout: Duration::from_secs(10),
    };
    let a = Arc::new(QuicTransport::with_socket(sa, ia, trust.clone(), opts.clone()).unwrap());
    let b = Arc::new(QuicTransport::with_socket(sb, ib, trust, opts).unwrap());

    let acceptor = {
        let b = b.clone();
        tokio::spawn(async move {
            let mut conns = Vec::new();
            for _ in 0..2 {
                conns.push(b.accept_quic().await.unwrap().unwrap());
            }
            conns
        })
    };
    let ia = a
        .connect_quic(
            b_addr,
            ExpectedPeer::Device(id_b),
            ConnectionKind::Interactive,
        )
        .await
        .unwrap();
    let bulk = a
        .connect_quic(b_addr, ExpectedPeer::Device(id_b), kind)
        .await
        .unwrap();
    let accepted = acceptor.await.unwrap();

    // B echoes pings and drains file data on whichever connection they arrive.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    for conn in accepted {
        let conn = Arc::new(conn);
        let c = conn.clone();
        tokio::spawn(async move {
            let Ok((mut tx, mut rx)) = c.accept_bi().await else {
                return;
            };
            let mut buf = [0u8; 8];
            loop {
                let mut got = 0;
                while got < 8 {
                    match rx.read(&mut buf[got..]).await {
                        Ok(Some(n)) => got += n,
                        _ => return,
                    }
                }
                if tx.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            let Ok(mut rx) = conn.accept_uni().await else {
                return;
            };
            let mut buf = vec![0u8; 256 * 1024];
            let mut n = 0u64;
            while let Ok(Some(k)) = rx.read(&mut buf).await {
                n += k as u64;
            }
            let _ = done_tx.send(n);
        });
    }
    let drained = tokio::spawn(async move { done_rx.recv().await.unwrap_or(0) });

    let (mut ptx, mut prx) = ia.open_bi(100).await.unwrap();
    let mut ping = async |samples: &mut Vec<Duration>| {
        let t = Instant::now();
        ptx.write_all(&[1; 8]).await.unwrap();
        let mut buf = [0u8; 8];
        let mut got = 0;
        while got < 8 {
            got += prx.read(&mut buf[got..]).await.unwrap().unwrap();
        }
        samples.push(t.elapsed());
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut base = Vec::new();
    for _ in 0..50 {
        ping(&mut base).await;
    }

    let start = Instant::now();
    let sender = tokio::spawn(async move {
        let mut tx = bulk.open_uni(0).await.unwrap();
        let chunk = vec![0x5au8; 256 * 1024];
        let mut sent = 0u64;
        while sent < total {
            let n = chunk.len().min((total - sent) as usize);
            tx.write_all(&chunk[..n]).await.unwrap();
            sent += n as u64;
        }
        tx.finish().unwrap();
        // Keep the connection alive until the receiver has everything.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(bulk);
    });
    let mut load = Vec::new();
    // Skip the first second: slow start probes before the delay signal exists.
    while !drained.is_finished() {
        let mut s = Vec::new();
        ping(&mut s).await;
        if start.elapsed() > Duration::from_secs(1) {
            load.extend(s);
        }
    }
    let received = drained.await.unwrap();
    let elapsed = start.elapsed();
    sender.abort();
    assert_eq!(received, total);
    Run {
        base_p50: percentile(&mut base, 0.5),
        base_p99: percentile(&mut base, 0.99),
        load_p99: percentile(&mut load, 0.99),
        rate_bps: total as f64 * 8.0 / elapsed.as_secs_f64(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_traffic_does_not_queue_in_front_of_input() {
    bridge_testkit::fine_timers();
    let long = std::env::var_os("BRIDGE_LONG_TESTS").is_some();
    let total = if long { 2 << 30 } else { 24 << 20 };
    let ledbat = run(ConnectionKind::Bulk, total, 11).await;
    eprintln!(
        "bulk (LEDBAT): idle p50 {:?} / p99 {:?}, p99 under load {:?}, {:.1} Mbit/s",
        ledbat.base_p50,
        ledbat.base_p99,
        ledbat.load_p99,
        ledbat.rate_bps / 1e6
    );
    // The simulator runs on OS timers. On Windows they get coarse under load (on battery,
    // timer coalescing), which reads as queuing delay to the controller; absolute limits
    // are checked on Linux and with BRIDGE_STRICT_NETSIM=1, the comparison everywhere.
    let strict = cfg!(not(windows)) || std::env::var_os("BRIDGE_STRICT_NETSIM").is_some();
    if strict {
        assert!(
            ledbat.load_p99 <= ledbat.base_p50 + Duration::from_millis(25),
            "input p99 grew from {:?} to {:?}",
            ledbat.base_p50,
            ledbat.load_p99
        );
        assert!(
            ledbat.rate_bps > 0.5 * 40e6,
            "the transfer still uses the link: {:.1} Mbit/s",
            ledbat.rate_bps / 1e6
        );
    }

    if !long {
        // Control: the default controller fills the 1 MB queue (≈ 200 ms at 40 Mbit/s).
        let cubic = run(ConnectionKind::Interactive, total, 12).await;
        eprintln!(
            "same transfer on the default controller: p99 {:?}",
            cubic.load_p99
        );
        assert!(
            cubic.load_p99 > ledbat.load_p99,
            "the delay-based controller is what helps"
        );
    }
}
