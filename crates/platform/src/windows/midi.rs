//! Windows MIDI through WinMM (`midir`). WinMM cannot create ports, so the virtual
//! ports are loopMIDI ports the user creates once (ADR-0009): `Bridge In` (we write,
//! the DAW reads) and `Bridge Out` (the DAW writes, we read). Windows MIDI Services
//! would make that step unnecessary (TODO(M7+), LIMITATIONS).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

use crate::{MidiIn, MidiOut, MidiPorts, MidiSink, PlatformError};

pub(crate) const LOOP_IN: &str = "Bridge In";
pub(crate) const LOOP_OUT: &str = "Bridge Out";
const CLIENT: &str = "Bridge";

fn os(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Os(e.to_string())
}

/// loopMIDI and WinMM may add a number to the name (`Bridge In 1`).
pub(crate) fn is_ours(name: &str) -> bool {
    name.starts_with(LOOP_IN) || name.starts_with(LOOP_OUT)
}

fn missing(port: &str) -> PlatformError {
    PlatformError::Unsupported(format!(
        "create a loopMIDI port named \"{port}\" (https://www.tobias-erichsen.de/software/loopmidi.html), \
         then restart Bridge"
    ))
}

struct Input(#[allow(dead_code)] MidiInputConnection<()>);
impl MidiIn for Input {}

struct Output(Arc<Mutex<MidiOutputConnection>>);

impl MidiOut for Output {
    fn send(&mut self, msg: &[u8]) -> Result<(), PlatformError> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(msg)
            .map_err(os)
    }
}

/// WinMM ports open once per process; outputs are shared between users.
#[derive(Default)]
pub struct WinMidi {
    outputs: Mutex<HashMap<String, Arc<Mutex<MidiOutputConnection>>>>,
}

impl WinMidi {
    pub fn new() -> Self {
        Self::default()
    }

    fn output_by(
        &self,
        pick: impl Fn(&str) -> bool,
    ) -> Result<Option<Box<dyn MidiOut>>, PlatformError> {
        let out = MidiOutput::new(CLIENT).map_err(os)?;
        let Some((port, name)) = out
            .ports()
            .into_iter()
            .filter_map(|p| out.port_name(&p).ok().map(|n| (p, n)))
            .find(|(_, n)| pick(n))
        else {
            return Ok(None);
        };
        let mut cache = self.outputs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache.get(&name) {
            return Ok(Some(Box::new(Output(c.clone()))));
        }
        let conn = Arc::new(Mutex::new(out.connect(&port, CLIENT).map_err(os)?));
        cache.insert(name, conn.clone());
        Ok(Some(Box::new(Output(conn))))
    }

    fn input_by(
        &self,
        pick: impl Fn(&str) -> bool,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Option<Box<dyn MidiIn>>, PlatformError> {
        let mut input = MidiInput::new(CLIENT).map_err(os)?;
        input.ignore(Ignore::None);
        let Some(port) = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).is_ok_and(|n| pick(&n)))
        else {
            return Ok(None);
        };
        let conn = input
            .connect(
                &port,
                CLIENT,
                move |_ts, msg, _| sink.on_midi(Instant::now(), msg),
                (),
            )
            .map_err(os)?;
        Ok(Some(Box::new(Input(conn))))
    }
}

impl MidiPorts for WinMidi {
    fn inputs(&self) -> Vec<String> {
        MidiInput::new(CLIENT)
            .map(|i| {
                i.ports()
                    .iter()
                    .filter_map(|p| i.port_name(p).ok())
                    .filter(|n| !is_ours(n))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn outputs(&self) -> Vec<String> {
        MidiOutput::new(CLIENT)
            .map(|o| {
                o.ports()
                    .iter()
                    .filter_map(|p| o.port_name(p).ok())
                    .filter(|n| !is_ours(n))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_input(
        &self,
        name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError> {
        self.input_by(|n| n == name, sink)?
            .ok_or_else(|| PlatformError::Os(format!("no MIDI input {name}")))
    }

    fn open_output(&self, name: &str) -> Result<Box<dyn MidiOut>, PlatformError> {
        self.output_by(|n| n == name)?
            .ok_or_else(|| PlatformError::Os(format!("no MIDI output {name}")))
    }

    fn virtual_source(&self, _name: &str) -> Result<Box<dyn MidiOut>, PlatformError> {
        self.output_by(|n| n.starts_with(LOOP_IN))?
            .ok_or_else(|| missing(LOOP_IN))
    }

    fn virtual_destination(
        &self,
        _name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError> {
        self.input_by(|n| n.starts_with(LOOP_OUT), sink)?
            .ok_or_else(|| missing(LOOP_OUT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopmidi_ports_are_recognised() {
        assert!(is_ours("Bridge In"));
        assert!(is_ours("Bridge Out 2"));
        assert!(!is_ours("Keystation 49"));
        assert!(
            matches!(missing(LOOP_IN), PlatformError::Unsupported(m) if m.contains("loopMIDI"))
        );
    }
}
