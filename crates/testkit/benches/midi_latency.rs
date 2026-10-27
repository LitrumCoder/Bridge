use std::time::Duration;

use bridge_testkit::midi_pair::MidiPair;
use criterion::{Criterion, criterion_group, criterion_main};

fn note_instrument_to_daw(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let pair = rt.block_on(MidiPair::connected());
    let mut group = c.benchmark_group("midi");
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(30);
    group.bench_function("note_instrument_to_daw_loopback", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    total += pair.note_latency(36 + (i % 48) as u8).await;
                }
                total
            })
        })
    });
    group.finish();
    rt.block_on(pair.shutdown());
}

criterion_group!(benches, note_instrument_to_daw);
criterion_main!(benches);
