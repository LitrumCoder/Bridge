//! Capture-to-injection latency percentiles over real UDP on loopback (ADR-0014 targets:
//! p50 < 0.5 ms, p99 < 2 ms on the reference machine; CI allows a generous margin).

#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::time::Duration;

use bridge_protocol::input::HidUsage;
use bridge_testkit::loopback::InputPair;

const SAMPLES: usize = 300;
/// Shared CI runners are noisy; the strict target is checked by the criterion bench.
const P99_LIMIT: Duration = Duration::from_millis(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_latency_percentiles_stay_within_target() {
    let pair = InputPair::connected().await;
    for _ in 0..20 {
        pair.key_latency(HidUsage::A).await;
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        samples.push(pair.key_latency(HidUsage(0x04 + (i % 20) as u16)).await);
    }
    samples.sort();
    let p = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    let (p50, p99) = (p(0.50), p(0.99));
    eprintln!(
        "input latency over loopback: p50 {p50:?}, p99 {p99:?}, max {:?}",
        samples.last().unwrap()
    );
    assert!(p99 < P99_LIMIT, "p99 {p99:?} exceeds {P99_LIMIT:?}");
    pair.shutdown().await;
}
