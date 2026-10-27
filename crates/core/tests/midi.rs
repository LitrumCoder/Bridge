//! Properties of the MIDI logic (ADR-0009): the jitter buffer never reorders or plays
//! early, the parser gives back exactly the messages that were sent, and the note
//! tracker leaves nothing sounding.

use bridge_core::midi::{JitterBuffer, JitterConfig, MidiParser, NoteTracker, is_valid_message};
use proptest::prelude::*;

fn message() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        (0x80u8..0xF0, 0u8..128, 0u8..128).prop_map(|(s, a, b)| {
            if (0xC0..0xE0).contains(&s) {
                vec![s, a]
            } else {
                vec![s, a, b]
            }
        }),
        (0xF8u8..=0xFF)
            .prop_filter("defined real-time", |b| *b != 0xF9 && *b != 0xFD)
            .prop_map(|b| vec![b]),
        proptest::collection::vec(0u8..128, 0..40).prop_map(|d| {
            let mut m = vec![0xF0];
            m.extend(d);
            m.push(0xF7);
            m
        }),
    ]
}

proptest! {
    #[test]
    fn jitter_buffer_is_monotonic_and_never_early(
        steps in proptest::collection::vec((0u64..5_000, 0u64..30_000, any::<bool>()), 1..200),
        adaptive in any::<bool>(),
    ) {
        let mut j = JitterBuffer::new(JitterConfig { adaptive, ..JitterConfig::default() });
        let (mut now, mut sent) = (100_000u64, 0u64);
        let mut last = 0;
        let mut pushed = Vec::new();
        for (gap, transit, burst) in steps {
            sent += if burst { 0 } else { gap };
            now = now.max(sent + transit);
            let play = j.push(now, sent, vec![0xF8]);
            prop_assert!(play >= now, "never scheduled in the past");
            prop_assert!(play >= last, "order is kept");
            last = play;
            pushed.push(play);
        }
        let mut played = 0;
        let mut t = 0;
        while let Some(next) = j.next_due() {
            prop_assert!(next >= t);
            t = next;
            prop_assert!(j.pop_due(next).is_some());
            played += 1;
        }
        prop_assert_eq!(played, pushed.len());
    }

    #[test]
    fn parser_returns_every_message_exactly(msgs in proptest::collection::vec(message(), 0..50), split in 1usize..7) {
        let bytes: Vec<u8> = msgs.concat();
        let mut p = MidiParser::new();
        let mut out = Vec::new();
        for chunk in bytes.chunks(split) {
            out.extend(p.feed(chunk));
        }
        prop_assert_eq!(&out, &msgs);
        prop_assert!(out.iter().all(|m| is_valid_message(m)));
    }

    #[test]
    fn released_tracker_is_silent(msgs in proptest::collection::vec(message(), 0..100)) {
        let mut t = NoteTracker::default();
        for m in &msgs {
            t.observe(m);
        }
        let offs = t.release_all();
        prop_assert!(t.is_silent());
        let mut check = NoteTracker::default();
        for m in msgs.iter().chain(&offs) {
            check.observe(m);
        }
        prop_assert!(check.is_silent(), "the releases undo every held note and pedal");
    }
}
