use std::hint::black_box;

use bridge_protocol::control::{ClockPong, ControlMessage};
use bridge_protocol::{ChannelClass, Envelope, FrameDecoder, encode_frame};
use criterion::{Criterion, criterion_group, criterion_main};

fn small_message_roundtrip(c: &mut Criterion) {
    let msg = ControlMessage::from(ClockPong {
        t0: 1_000_000,
        t1: 1_000_500,
        t2: 1_000_520,
    });
    let limit = ChannelClass::Control.max_frame_len();
    let mut wire = Vec::with_capacity(64);
    let mut dec = FrameDecoder::new(limit);

    c.benchmark_group("codec")
        .bench_function("clock_pong_roundtrip", |b| {
            b.iter(|| {
                wire.clear();
                let env = black_box(&msg).to_envelope().expect("encode");
                encode_frame(&env.to_bytes(), limit, &mut wire).expect("frame");
                dec.push(&wire);
                let frame = dec.next_frame().expect("frame").expect("complete");
                let parsed = Envelope::parse(&frame).expect("envelope");
                black_box(ControlMessage::from_envelope(&parsed).expect("decode"))
            })
        });
}

criterion_group!(benches, small_message_roundtrip);
criterion_main!(benches);
