//! Arbitrary bytes from a peer must never panic any decoder (complements the fuzz targets,
//! runs on stable in every CI job).

use bridge_protocol::control::ControlMessage;
use bridge_protocol::{ChannelClass, Envelope, FrameDecoder, decode_datagram};
use proptest::prelude::*;

fn decode_everything(bytes: &[u8]) {
    for class in ChannelClass::ALL {
        let mut dec = FrameDecoder::new(class.max_frame_len());
        dec.push(bytes);
        while let Ok(Some(frame)) = dec.next_frame() {
            if let Ok(env) = Envelope::parse(&frame) {
                let _ = ControlMessage::from_envelope(&env);
            }
        }
    }
    if let Ok(env) = Envelope::parse(bytes) {
        let _ = ControlMessage::decode(env.kind, env.payload);
    }
    let _ = decode_datagram(bytes);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        decode_everything(&bytes);
    }

    #[test]
    fn random_payload_for_every_kind_never_panics(
        kind in 0u16..32,
        payload in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let _ = ControlMessage::decode(kind, &payload);
        let _ = bridge_protocol::input::InputMessage::decode(kind, &payload);
        let _ = bridge_protocol::decode_payload::<bridge_protocol::input::Motion>(&payload);
    }
}
