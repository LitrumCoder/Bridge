#![no_main]

use bridge_protocol::control::ControlMessage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let kind = u16::from_le_bytes([data[0], data[1]]) % 16;
    if let Ok(msg) = ControlMessage::decode(kind, &data[2..]) {
        let env = msg.to_envelope().expect("decoded message must re-encode");
        assert_eq!(ControlMessage::decode(env.kind, &env.payload).expect("roundtrip"), msg);
    }
});
