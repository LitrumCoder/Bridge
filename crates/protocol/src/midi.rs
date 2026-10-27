//! Messages of MIDI forwarding (`FeatureId::MIDI`, ADR-0009). Raw MIDI 1.0 messages
//! with the sender's capture time; every message is checked to be complete and
//! well-formed before it can reach a virtual port.

use serde::{Deserialize, Serialize};

use crate::{
    BoundedBytes, BoundedString, BoundedVec, DecodeError, FeatureId, Validate, define_messages,
};

pub const MAX_SYSEX: usize = 64 * 1024;
pub const MAX_PORTS: usize = 32;
pub const MAX_BATCH: usize = 256;

/// Data bytes after a status byte; `None` for SysEx.
pub fn data_len(status: u8) -> Option<usize> {
    match status {
        0x80..=0xBF | 0xE0..=0xEF => Some(2),
        0xC0..=0xDF | 0xF1 | 0xF3 => Some(1),
        0xF2 => Some(2),
        0xF0 => None,
        _ => Some(0),
    }
}

/// One complete MIDI 1.0 message (status first, SysEx terminated, no stray statuses).
pub fn is_valid_message(msg: &[u8]) -> bool {
    let Some(&status) = msg.first() else {
        return false;
    };
    if status & 0x80 == 0 || status == 0xF7 {
        return false;
    }
    match data_len(status) {
        None => {
            msg.len() >= 2
                && msg.len() <= MAX_SYSEX
                && msg[msg.len() - 1] == 0xF7
                && msg[1..msg.len() - 1].iter().all(|b| b & 0x80 == 0)
        }
        Some(n) => msg.len() == n + 1 && msg[1..].iter().all(|b| b & 0x80 == 0),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PortDirection {
    /// An instrument's output on the sender: the receiver offers it to its apps as an
    /// input.
    FromInstrument,
    /// An instrument's input on the sender: the receiver's apps can play it.
    ToInstrument,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PortInfo {
    pub id: u16,
    pub name: BoundedString<64>,
    pub direction: PortDirection,
}

/// The ports the sender shares; replaces any earlier list.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Ports {
    /// The sender's device name, for port names like "Keystation via Studio PC".
    pub device: BoundedString<64>,
    pub ports: BoundedVec<PortInfo, MAX_PORTS>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// Capture time on the sender's clock, microseconds.
    pub ts: u64,
    pub data: BoundedBytes<MAX_SYSEX>,
}

/// Messages for port `port`: from an instrument (a `FromInstrument` port of the
/// sender) or, with `to_instrument`, towards one (a `ToInstrument` port of the
/// receiver). Port ids belong to the device that owns the instrument.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Events {
    pub port: u16,
    pub to_instrument: bool,
    pub events: BoundedVec<Event, MAX_BATCH>,
}

define_messages! {
    pub enum MidiMessage for FeatureId::MIDI {
        1 => Ports,
        2 => Events,
    }
}

impl Validate for Ports {
    fn validate(&self) -> Result<(), DecodeError> {
        for (i, p) in self.ports.iter().enumerate() {
            if self.ports[..i].iter().any(|q| q.id == p.id) {
                return Err(DecodeError::Invalid("duplicate MIDI port id"));
            }
            if p.name
                .chars()
                .chain(self.device.chars())
                .any(char::is_control)
            {
                return Err(DecodeError::Invalid("control characters in a port name"));
            }
        }
        Ok(())
    }
}

impl Validate for Events {
    fn validate(&self) -> Result<(), DecodeError> {
        if !self.events.iter().all(|e| is_valid_message(&e.data)) {
            return Err(DecodeError::Invalid("malformed MIDI message"));
        }
        if self.events.windows(2).any(|w| w[1].ts < w[0].ts) {
            return Err(DecodeError::Invalid("MIDI events out of order"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Envelope;

    fn roundtrip(m: MidiMessage) -> Result<MidiMessage, DecodeError> {
        let bytes = m.to_envelope().expect("encode").to_bytes();
        MidiMessage::from_envelope(&Envelope::parse(&bytes).expect("parse"))
    }

    fn ev(ts: u64, d: &[u8]) -> Event {
        Event {
            ts,
            data: BoundedBytes::new(d.to_vec()).expect("fits"),
        }
    }

    #[test]
    fn events_must_be_whole_and_ordered() {
        let ok = Events {
            port: 1,
            to_instrument: false,
            events: BoundedVec::try_from_vec(vec![
                ev(1, &[0x90, 60, 100]),
                ev(2, &[0xF0, 1, 0xF7]),
                ev(2, &[0xF8]),
            ])
            .expect("fits"),
        };
        assert!(roundtrip(ok.into()).is_ok());
        for bad in [
            vec![ev(1, &[0x90, 60])],
            vec![ev(1, &[60, 1, 2])],
            vec![ev(1, &[0xF7])],
            vec![ev(5, &[0xF8]), ev(4, &[0xF8])],
        ] {
            let m = Events {
                port: 1,
                to_instrument: true,
                events: BoundedVec::try_from_vec(bad).expect("fits"),
            };
            assert!(roundtrip(m.into()).is_err());
        }
    }

    #[test]
    fn port_lists_are_checked() {
        let p = |id, name: &str| PortInfo {
            id,
            name: BoundedString::truncating(name),
            direction: PortDirection::FromInstrument,
        };
        let ok = Ports {
            device: BoundedString::truncating("Studio PC"),
            ports: BoundedVec::try_from_vec(vec![p(1, "Keystation 49"), p(2, "Launchpad")])
                .expect("fits"),
        };
        assert!(roundtrip(ok.into()).is_ok());
        let dup = Ports {
            device: BoundedString::default(),
            ports: BoundedVec::try_from_vec(vec![p(1, "a"), p(1, "b")]).expect("fits"),
        };
        assert!(roundtrip(dup.into()).is_err());
        let ctl = Ports {
            device: BoundedString::default(),
            ports: BoundedVec::try_from_vec(vec![p(1, "a\nb")]).expect("fits"),
        };
        assert!(roundtrip(ctl.into()).is_err());
    }
}
