//! ALSA sequencer MIDI (ADR-0009), loaded from the system `libasound.so.2` at run time
//! (ADR-0013: OS libraries are linked dynamically; building needs no ALSA headers).
//! One sequencer handle sends, another one receives on its own thread; virtual ports
//! are ordinary sequencer ports that DAWs (and JACK/PipeWire bridges) see by name.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bridge_core::midi::{MAX_SYSEX, MidiParser};

use crate::{MidiIn, MidiOut, MidiPorts, MidiSink, PlatformError};

const OPEN_DUPLEX: c_int = 3;
const CAP_READ: c_uint = 1 << 0;
const CAP_WRITE: c_uint = 1 << 1;
const CAP_SUBS_READ: c_uint = 1 << 5;
const CAP_SUBS_WRITE: c_uint = 1 << 6;
const TYPE_MIDI_GENERIC: c_uint = 1 << 1;
const TYPE_APPLICATION: c_uint = 1 << 20;
const EVENT_NONE: u8 = 255;
const EVENT_USR0: u8 = 90;
const QUEUE_DIRECT: u8 = 253;
const ADDRESS_SUBSCRIBERS: u8 = 254;
const ADDRESS_UNKNOWN: u8 = 253;

/// `snd_seq_event_t`: 28 bytes, the data union is packed.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeqEvent {
    ty: u8,
    flags: u8,
    tag: u8,
    queue: u8,
    time: [u32; 2],
    src_client: u8,
    src_port: u8,
    dst_client: u8,
    dst_port: u8,
    data: [u8; 12],
}

type H = *mut c_void;

/// A `dlopen` handle kept for the life of the process (symbols point into it).
struct Library(*mut c_void);

// SAFETY: a dlopen handle is only read (dlsym) and never closed.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

impl Library {
    fn open(name: &CStr) -> Result<Self, String> {
        // SAFETY: plain dlopen of a system library.
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            // SAFETY: dlerror returns a static, NUL-terminated message or NULL.
            let e = unsafe { libc::dlerror() };
            let msg = if e.is_null() {
                "not found".to_owned()
            } else {
                // SAFETY: as above.
                unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
            };
            return Err(msg);
        }
        Ok(Self(h))
    }

    /// # Safety
    /// `T` must be the function pointer type of the symbol.
    unsafe fn get<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        // SAFETY: valid handle and name.
        let p = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if p.is_null() {
            return Err("missing symbol".into());
        }
        // SAFETY: the caller states that `T` is the symbol's pointer type.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }
}

struct Api {
    _lib: Library,
    seq_open: unsafe extern "C" fn(*mut H, *const c_char, c_int, c_int) -> c_int,
    set_client_name: unsafe extern "C" fn(H, *const c_char) -> c_int,
    client_id: unsafe extern "C" fn(H) -> c_int,
    create_simple_port: unsafe extern "C" fn(H, *const c_char, c_uint, c_uint) -> c_int,
    connect_from: unsafe extern "C" fn(H, c_int, c_int, c_int) -> c_int,
    connect_to: unsafe extern "C" fn(H, c_int, c_int, c_int) -> c_int,
    output_direct: unsafe extern "C" fn(H, *mut SeqEvent) -> c_int,
    event_input: unsafe extern "C" fn(H, *mut *mut SeqEvent) -> c_int,
    midi_new: unsafe extern "C" fn(usize, *mut H) -> c_int,
    midi_encode: unsafe extern "C" fn(H, *const u8, c_long, *mut SeqEvent) -> c_long,
    midi_decode: unsafe extern "C" fn(H, *mut u8, c_long, *const SeqEvent) -> c_long,
    midi_reset_encode: unsafe extern "C" fn(H),
    midi_no_status: unsafe extern "C" fn(H, c_int),
    client_info_malloc: unsafe extern "C" fn(*mut H) -> c_int,
    client_info_set_client: unsafe extern "C" fn(H, c_int),
    client_info_get_client: unsafe extern "C" fn(H) -> c_int,
    query_next_client: unsafe extern "C" fn(H, H) -> c_int,
    port_info_malloc: unsafe extern "C" fn(*mut H) -> c_int,
    port_info_set_client: unsafe extern "C" fn(H, c_int),
    port_info_set_port: unsafe extern "C" fn(H, c_int),
    port_info_get_port: unsafe extern "C" fn(H) -> c_int,
    port_info_get_capability: unsafe extern "C" fn(H) -> c_uint,
    port_info_get_name: unsafe extern "C" fn(H) -> *const c_char,
    query_next_port: unsafe extern "C" fn(H, H) -> c_int,
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        // SAFETY: the symbol is declared with the signature of alsa/seq.h.
        unsafe { $lib.get(&CString::new($name).unwrap_or_default()) }
            .map_err(|e| PlatformError::Unsupported(format!("libasound lacks {}: {e}", $name)))?
    };
}

impl Api {
    fn load() -> Result<Self, PlatformError> {
        // SAFETY: loading the system ALSA library runs its initialisers, nothing else.
        let lib = Library::open(c"libasound.so.2").map_err(|e| {
            PlatformError::Unsupported(format!("ALSA is not installed (libasound.so.2): {e}"))
        })?;
        Ok(Self {
            seq_open: sym!(lib, "snd_seq_open"),
            set_client_name: sym!(lib, "snd_seq_set_client_name"),
            client_id: sym!(lib, "snd_seq_client_id"),
            create_simple_port: sym!(lib, "snd_seq_create_simple_port"),
            connect_from: sym!(lib, "snd_seq_connect_from"),
            connect_to: sym!(lib, "snd_seq_connect_to"),
            output_direct: sym!(lib, "snd_seq_event_output_direct"),
            event_input: sym!(lib, "snd_seq_event_input"),
            midi_new: sym!(lib, "snd_midi_event_new"),
            midi_encode: sym!(lib, "snd_midi_event_encode"),
            midi_decode: sym!(lib, "snd_midi_event_decode"),
            midi_reset_encode: sym!(lib, "snd_midi_event_reset_encode"),
            midi_no_status: sym!(lib, "snd_midi_event_no_status"),
            client_info_malloc: sym!(lib, "snd_seq_client_info_malloc"),
            client_info_set_client: sym!(lib, "snd_seq_client_info_set_client"),
            client_info_get_client: sym!(lib, "snd_seq_client_info_get_client"),
            query_next_client: sym!(lib, "snd_seq_query_next_client"),
            port_info_malloc: sym!(lib, "snd_seq_port_info_malloc"),
            port_info_set_client: sym!(lib, "snd_seq_port_info_set_client"),
            port_info_set_port: sym!(lib, "snd_seq_port_info_set_port"),
            port_info_get_port: sym!(lib, "snd_seq_port_info_get_port"),
            port_info_get_capability: sym!(lib, "snd_seq_port_info_get_capability"),
            port_info_get_name: sym!(lib, "snd_seq_port_info_get_name"),
            query_next_port: sym!(lib, "snd_seq_query_next_port"),
            _lib: lib,
        })
    }

    fn open(&self, name: &str) -> Result<H, PlatformError> {
        let mut seq: H = std::ptr::null_mut();
        let default = c"default";
        // SAFETY: out-pointer to a local; strings are NUL-terminated.
        let r = unsafe { (self.seq_open)(&mut seq, default.as_ptr(), OPEN_DUPLEX, 0) };
        if r < 0 || seq.is_null() {
            return Err(PlatformError::Unsupported(format!(
                "cannot open the ALSA sequencer ({r})"
            )));
        }
        let n = CString::new(name).unwrap_or_default();
        // SAFETY: valid handle and string.
        unsafe { (self.set_client_name)(seq, n.as_ptr()) };
        Ok(seq)
    }

    fn port(&self, seq: H, name: &str, caps: c_uint) -> Result<c_int, PlatformError> {
        let n = CString::new(name).map_err(|_| PlatformError::Os("bad port name".into()))?;
        // SAFETY: valid handle and string.
        let p = unsafe {
            (self.create_simple_port)(seq, n.as_ptr(), caps, TYPE_MIDI_GENERIC | TYPE_APPLICATION)
        };
        if p < 0 {
            return Err(PlatformError::Os(format!(
                "cannot create MIDI port {name} ({p})"
            )));
        }
        Ok(p)
    }

    fn coder(&self) -> Result<H, PlatformError> {
        let mut dev: H = std::ptr::null_mut();
        // SAFETY: out-pointer to a local.
        if unsafe { (self.midi_new)(MAX_SYSEX, &mut dev) } < 0 || dev.is_null() {
            return Err(PlatformError::Os("cannot create an ALSA MIDI coder".into()));
        }
        // SAFETY: valid coder.
        unsafe { (self.midi_no_status)(dev, 1) };
        Ok(dev)
    }

    /// `(client, port, caps, name)` of every port of other clients.
    fn list(&self, seq: H) -> Vec<(c_int, c_int, c_uint, String)> {
        let mut out = Vec::new();
        let (mut ci, mut pi): (H, H) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the info objects are allocated by ALSA and only used through its API;
        // they are leaked deliberately (a few bytes per listing).
        unsafe {
            if (self.client_info_malloc)(&mut ci) < 0 || (self.port_info_malloc)(&mut pi) < 0 {
                return out;
            }
            let me = (self.client_id)(seq);
            (self.client_info_set_client)(ci, -1);
            while (self.query_next_client)(seq, ci) >= 0 {
                let client = (self.client_info_get_client)(ci);
                if client == me || client == 0 {
                    continue;
                }
                (self.port_info_set_client)(pi, client);
                (self.port_info_set_port)(pi, -1);
                while (self.query_next_port)(seq, pi) >= 0 {
                    let name = CStr::from_ptr((self.port_info_get_name)(pi))
                        .to_string_lossy()
                        .into_owned();
                    out.push((
                        client,
                        (self.port_info_get_port)(pi),
                        (self.port_info_get_capability)(pi),
                        name,
                    ));
                }
            }
        }
        out
    }
}

struct Seq(H);
// SAFETY: each handle is used either under a mutex (output) or by its one thread (input).
unsafe impl Send for Seq {}
// SAFETY: as above.
unsafe impl Sync for Seq {}

type Listener = (Arc<dyn MidiSink>, MidiParser);

struct Shared {
    api: Api,
    out: Mutex<(Seq, Seq)>,
    input: Seq,
    in_client: c_int,
    wake_port: c_int,
    sinks: Mutex<HashMap<c_int, Listener>>,
    stop: AtomicBool,
}

pub struct AlsaMidi(Arc<Shared>);

fn receive(s: Arc<Shared>) {
    let coder = match s.api.coder() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut buf = vec![0u8; MAX_SYSEX];
    while !s.stop.load(Ordering::Acquire) {
        let mut ev: *mut SeqEvent = std::ptr::null_mut();
        // SAFETY: blocking read on the handle owned by this thread.
        if unsafe { (s.api.event_input)(s.input.0, &mut ev) } < 0 || ev.is_null() {
            continue;
        }
        let at = Instant::now();
        // SAFETY: ALSA returns a pointer to an event it owns until the next read.
        let e = unsafe { *ev };
        if e.dst_port as c_int == s.wake_port {
            continue;
        }
        // SAFETY: decoding into our buffer of the stated size.
        let n = unsafe { (s.api.midi_decode)(coder, buf.as_mut_ptr(), buf.len() as c_long, ev) };
        if n <= 0 {
            continue;
        }
        let mut sinks = s.sinks.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((sink, parser)) = sinks.get_mut(&(e.dst_port as c_int)) {
            for msg in parser.feed(&buf[..n as usize]) {
                sink.on_midi(at, &msg);
            }
        }
    }
}

impl AlsaMidi {
    pub fn open() -> Result<Self, PlatformError> {
        let api = Api::load()?;
        let out_seq = api.open("Bridge")?;
        let input = api.open("Bridge")?;
        let coder = api.coder()?;
        // SAFETY: valid handle.
        let in_client = unsafe { (api.client_id)(input) };
        let wake_port = api.port(input, "bridge-wake", CAP_WRITE)?;
        let shared = Arc::new(Shared {
            api,
            out: Mutex::new((Seq(out_seq), Seq(coder))),
            input: Seq(input),
            in_client,
            wake_port,
            sinks: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("bridge-alsa-midi".into())
            .spawn(move || receive(s))
            .map_err(|e| PlatformError::Os(e.to_string()))?;
        Ok(Self(shared))
    }

    fn find(&self, name: &str, caps: c_uint) -> Option<(c_int, c_int)> {
        let out = self.0.out.lock().unwrap_or_else(|p| p.into_inner());
        self.0
            .api
            .list(out.0.0)
            .into_iter()
            .find(|(c, _, cap, n)| *c != self.0.in_client && cap & caps == caps && n == name)
            .map(|(c, p, _, _)| (c, p))
    }

    fn names(&self, caps: c_uint) -> Vec<String> {
        let out = self.0.out.lock().unwrap_or_else(|p| p.into_inner());
        self.0
            .api
            .list(out.0.0)
            .into_iter()
            .filter(|(c, _, cap, _)| *c != self.0.in_client && cap & caps == caps)
            .map(|(_, _, _, n)| n)
            .collect()
    }

    fn listen(&self, port: c_int, sink: Arc<dyn MidiSink>) -> Box<dyn MidiIn> {
        self.0
            .sinks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(port, (sink, MidiParser::new()));
        Box::new(Listening(self.0.clone(), port))
    }
}

impl Drop for AlsaMidi {
    fn drop(&mut self) {
        let s = &self.0;
        s.stop.store(true, Ordering::Release);
        // Wake the receiving thread with an event to its private port.
        let out = s.out.lock().unwrap_or_else(|p| p.into_inner());
        let mut ev = SeqEvent {
            ty: EVENT_USR0,
            queue: QUEUE_DIRECT,
            dst_client: s.in_client as u8,
            dst_port: s.wake_port as u8,
            ..SeqEvent::default()
        };
        // SAFETY: valid handle and event.
        unsafe { (s.api.output_direct)(out.0.0, &mut ev) };
    }
}

struct Listening(Arc<Shared>, c_int);

impl MidiIn for Listening {}

impl Drop for Listening {
    fn drop(&mut self) {
        self.0
            .sinks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.1);
    }
}

struct Sending(Arc<Shared>, c_int);

impl MidiOut for Sending {
    fn send(&mut self, msg: &[u8]) -> Result<(), PlatformError> {
        let s = &self.0;
        let out = s.out.lock().unwrap_or_else(|p| p.into_inner());
        let (seq, coder) = (out.0.0, out.1.0);
        // SAFETY: the coder belongs to this handle and is used under the mutex.
        unsafe { (s.api.midi_reset_encode)(coder) };
        let mut rest = msg;
        while !rest.is_empty() {
            let mut ev = SeqEvent {
                ty: EVENT_NONE,
                ..SeqEvent::default()
            };
            // SAFETY: encoding from our slice into a local event.
            let used =
                unsafe { (s.api.midi_encode)(coder, rest.as_ptr(), rest.len() as c_long, &mut ev) };
            if used <= 0 {
                return Err(PlatformError::Os(
                    "ALSA cannot encode this MIDI message".into(),
                ));
            }
            rest = &rest[used as usize..];
            if ev.ty == EVENT_NONE {
                continue;
            }
            ev.queue = QUEUE_DIRECT;
            ev.src_port = self.1 as u8;
            ev.dst_client = ADDRESS_SUBSCRIBERS;
            ev.dst_port = ADDRESS_UNKNOWN;
            // SAFETY: valid handle; SysEx data points into the coder, alive under the lock.
            if unsafe { (s.api.output_direct)(seq, &mut ev) } < 0 {
                return Err(PlatformError::Os("ALSA refused the MIDI event".into()));
            }
        }
        Ok(())
    }
}

impl MidiPorts for AlsaMidi {
    fn inputs(&self) -> Vec<String> {
        self.names(CAP_READ | CAP_SUBS_READ)
    }

    fn outputs(&self) -> Vec<String> {
        self.names(CAP_WRITE | CAP_SUBS_WRITE)
    }

    fn open_input(
        &self,
        name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError> {
        let (client, port) = self
            .find(name, CAP_READ | CAP_SUBS_READ)
            .ok_or_else(|| PlatformError::Os(format!("no MIDI input {name}")))?;
        let s = &self.0;
        let mine = s.api.port(s.input.0, &format!("from {name}"), CAP_WRITE)?;
        // SAFETY: valid handle and ports.
        if unsafe { (s.api.connect_from)(s.input.0, mine, client, port) } < 0 {
            return Err(PlatformError::Os(format!("cannot connect to {name}")));
        }
        Ok(self.listen(mine, sink))
    }

    fn open_output(&self, name: &str) -> Result<Box<dyn MidiOut>, PlatformError> {
        let (client, port) = self
            .find(name, CAP_WRITE | CAP_SUBS_WRITE)
            .ok_or_else(|| PlatformError::Os(format!("no MIDI output {name}")))?;
        let s = &self.0;
        let out = s.out.lock().unwrap_or_else(|p| p.into_inner());
        let mine = s.api.port(out.0.0, &format!("to {name}"), CAP_READ)?;
        // SAFETY: valid handle and ports.
        if unsafe { (s.api.connect_to)(out.0.0, mine, client, port) } < 0 {
            return Err(PlatformError::Os(format!("cannot connect to {name}")));
        }
        drop(out);
        Ok(Box::new(Sending(self.0.clone(), mine)))
    }

    fn virtual_source(&self, name: &str) -> Result<Box<dyn MidiOut>, PlatformError> {
        let out = self.0.out.lock().unwrap_or_else(|p| p.into_inner());
        let port = self.0.api.port(out.0.0, name, CAP_READ | CAP_SUBS_READ)?;
        drop(out);
        Ok(Box::new(Sending(self.0.clone(), port)))
    }

    fn virtual_destination(
        &self,
        name: &str,
        sink: Arc<dyn MidiSink>,
    ) -> Result<Box<dyn MidiIn>, PlatformError> {
        let port = self
            .0
            .api
            .port(self.0.input.0, name, CAP_WRITE | CAP_SUBS_WRITE)?;
        Ok(self.listen(port, sink))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout_matches_alsa() {
        assert_eq!(std::mem::size_of::<SeqEvent>(), 28);
        assert_eq!(std::mem::offset_of!(SeqEvent, data), 16);
        assert_eq!(std::mem::offset_of!(SeqEvent, src_client), 12);
    }

    /// Runs only where the sequencer exists (`/dev/snd/seq`); WSL and CI usually lack it.
    #[test]
    fn virtual_ports_loop_back_when_alsa_is_present() {
        let Ok(midi) = AlsaMidi::open() else { return };
        let _ = midi.inputs();
    }
}
