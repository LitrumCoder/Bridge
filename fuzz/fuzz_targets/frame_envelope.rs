#![no_main]

use bridge_protocol::control::ControlMessage;
use bridge_protocol::{ChannelClass, Envelope, FrameDecoder};
use libfuzzer_sys::fuzz_target;

// The first byte picks the channel limit and how the stream is chunked, so the
// decoder is exercised with partial prefixes and bodies.
fuzz_target!(|data: &[u8]| {
    let Some((&selector, stream)) = data.split_first() else {
        return;
    };
    let class = ChannelClass::ALL[usize::from(selector) % ChannelClass::ALL.len()];
    let chunk = usize::from(selector >> 3).max(1);

    let mut dec = FrameDecoder::new(class.max_frame_len());
    for piece in stream.chunks(chunk) {
        dec.push(piece);
        loop {
            match dec.next_frame() {
                Ok(Some(frame)) => check_frame(&frame),
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});

fn check_frame(frame: &[u8]) {
    let Ok(env) = Envelope::parse(frame) else {
        return;
    };
    if let Ok(msg) = ControlMessage::from_envelope(&env) {
        let bytes = msg.to_envelope().expect("decoded message must re-encode").to_bytes();
        let again = ControlMessage::from_envelope(&Envelope::parse(&bytes).expect("own envelope"))
            .expect("own encoding must decode");
        assert_eq!(again, msg);
    }
}
