use std::time::Duration;

use bridge_protocol::input::HidUsage;
use bridge_testkit::loopback::InputPair;
use criterion::{Criterion, criterion_group, criterion_main};

fn key_capture_to_injection(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let pair = rt.block_on(InputPair::connected());
    let mut group = c.benchmark_group("input");
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("key_capture_to_injection_loopback", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    total += pair.key_latency(HidUsage(0x04 + (i % 20) as u16)).await;
                }
                total
            })
        })
    });
    group.finish();
    rt.block_on(pair.shutdown());
}

criterion_group!(benches, key_capture_to_injection);
criterion_main!(benches);
