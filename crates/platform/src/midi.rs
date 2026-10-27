//! MIDI ports (ADR-0009): physical ports of instruments and virtual ports other
//! applications (a DAW) see as their own inputs and outputs.

use std::sync::Arc;
use std::time::Instant;

use crate::PlatformError;

/// Receives whole MIDI messages from a port, with the moment they were captured.
pub trait MidiSink: Send + Sync + 'static {
    fn on_midi(&self, at: Instant, msg: &[u8]);
}

pub trait MidiOut: Send + 'static {
    fn send(&mut self, msg: &[u8]) -> Result<(), PlatformError>;
}

/// An open input; dropping it closes the port.
pub trait MidiIn: Send + 'static {}

pub trait MidiPorts: Send + Sync + 'static {
    /// Ports applications can read from (instruments), ours excluded.
    fn inputs(&self) -> Vec<String>;
    /// Ports applications can write to (instruments, synths), ours excluded.
    fn outputs(&self) -> Vec<String>;
    fn open_input(
        &self,
        name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError>;
    fn open_output(&self, name: &str) -> Result<Box<dyn MidiOut>, PlatformError>;
    /// A port other applications see as an input; what we send there, they receive.
    fn virtual_source(&self, name: &str) -> Result<Box<dyn MidiOut>, PlatformError>;
    /// A port other applications see as an output; what they play arrives at `sink`.
    fn virtual_destination(
        &self,
        name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError>;
}

/// The MIDI services of this OS.
pub fn native_midi() -> Result<Arc<dyn MidiPorts>, PlatformError> {
    #[cfg(windows)]
    {
        Ok(Arc::new(crate::windows::WinMidi::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Arc::new(crate::linux_alsa::AlsaMidi::open()?))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // TODO(macOS-final): CoreMIDI virtual sources and destinations.
        Err(PlatformError::Unsupported(
            "MIDI forwarding is not available on this OS yet".into(),
        ))
    }
}
