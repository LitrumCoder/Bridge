#![no_main]

use bridge_protocol::{decode_datagram, encode_datagram};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((feature, payload)) = decode_datagram(data) {
        let again = encode_datagram(feature, payload);
        assert_eq!(decode_datagram(&again).expect("own datagram"), (feature, payload));
    }
});
