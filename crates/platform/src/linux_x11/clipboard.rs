//! X11 `CLIPBOARD` (ADR-0007): XFixes reports owner changes; owning the selection is
//! delayed rendering by nature, so a peer's offer is rendered only when a client asks
//! for a target. Large data goes through the INCR protocol both ways.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _,
    CreateWindowAux, EventMask, PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use crate::{ClipChange, ClipFormat, Clipboard, ClipboardProvider, ClipboardSink, PlatformError};

/// Largest selection we read from another client.
const READ_LIMIT: usize = 256 * 1024 * 1024;

fn os(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Os(e.to_string())
}

#[derive(Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text_plain: Atom,
    html: Atom,
    rtf: Atom,
    app_rtf: Atom,
    png: Atom,
    incr: Atom,
    kde_password: Atom,
    transfer: Atom,
}

impl Atoms {
    fn intern(c: &RustConnection) -> Result<Self, PlatformError> {
        let a = |name: &str| -> Result<Atom, PlatformError> {
            Ok(c.intern_atom(false, name.as_bytes())
                .map_err(os)?
                .reply()
                .map_err(os)?
                .atom)
        };
        Ok(Self {
            clipboard: a("CLIPBOARD")?,
            targets: a("TARGETS")?,
            utf8: a("UTF8_STRING")?,
            text_plain: a("text/plain;charset=utf-8")?,
            html: a("text/html")?,
            rtf: a("text/rtf")?,
            app_rtf: a("application/rtf")?,
            png: a("image/png")?,
            incr: a("INCR")?,
            kde_password: a("x-kde-passwordManagerHint")?,
            transfer: a("BRIDGE_CLIPBOARD")?,
        })
    }

    /// Targets for one wire format, preferred first.
    fn targets_of(&self, f: ClipFormat) -> Vec<Atom> {
        match f {
            ClipFormat::TEXT => vec![self.utf8, self.text_plain],
            ClipFormat::HTML => vec![self.html],
            ClipFormat::RTF => vec![self.rtf, self.app_rtf],
            ClipFormat::PNG => vec![self.png],
            _ => Vec::new(),
        }
    }

    fn format_of(&self, target: Atom) -> Option<ClipFormat> {
        [
            ClipFormat::TEXT,
            ClipFormat::HTML,
            ClipFormat::RTF,
            ClipFormat::PNG,
        ]
        .into_iter()
        .find(|f| self.targets_of(*f).contains(&target))
    }
}

/// HTML from Firefox and some others is UTF-16 with a BOM.
fn decode_html(bytes: &[u8]) -> Vec<u8> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&units).into_bytes();
    }
    bytes.to_vec()
}

/// A short-lived connection and window that asks the selection owner for one target.
struct Reader {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
}

impl Reader {
    fn new() -> Result<Self, PlatformError> {
        let (conn, screen) =
            x11rb::connect(None).map_err(|e| PlatformError::Unsupported(e.to_string()))?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().map_err(os)?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(os)?;
        let atoms = Atoms::intern(&conn)?;
        Ok(Self {
            conn,
            window,
            atoms,
        })
    }

    fn wait(&self, deadline: Instant, mut pick: impl FnMut(&Event) -> bool) -> Option<Event> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(e)) if pick(&e) => return Some(e),
                Ok(Some(_)) => {}
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                _ => return None,
            }
        }
    }

    fn take_property(&self) -> Option<(Atom, Vec<u8>)> {
        let r = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.transfer,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )
            .ok()?
            .reply()
            .ok()?;
        Some((r.type_, r.value))
    }

    /// Converts the clipboard to `target`; `None` if the owner refuses or times out.
    fn convert(&self, target: Atom, timeout: Duration) -> Option<Vec<u8>> {
        let a = self.atoms;
        self.conn
            .convert_selection(self.window, a.clipboard, target, a.transfer, CURRENT_TIME)
            .ok()?;
        self.conn.flush().ok()?;
        let deadline = Instant::now() + timeout;
        let Event::SelectionNotify(n) =
            self.wait(deadline, |e| matches!(e, Event::SelectionNotify(_)))?
        else {
            return None;
        };
        if n.property == NONE {
            return None;
        }
        let (ty, value) = self.take_property()?;
        self.conn.flush().ok()?;
        if ty != a.incr {
            return Some(value);
        }
        // INCR: each new value is a chunk; an empty one ends the transfer.
        let mut out = Vec::new();
        loop {
            self.wait(deadline, |e| {
                matches!(e, Event::PropertyNotify(p)
                    if p.atom == a.transfer && p.state == Property::NEW_VALUE)
            })?;
            let (_, chunk) = self.take_property()?;
            self.conn.flush().ok()?;
            if chunk.is_empty() {
                return Some(out);
            }
            if out.len() + chunk.len() > READ_LIMIT {
                return None;
            }
            out.extend_from_slice(&chunk);
        }
    }

    fn targets(&self) -> Vec<Atom> {
        self.convert(self.atoms.targets, Duration::from_secs(1))
            .map(|v| {
                v.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Outgoing {
    ty: Atom,
    data: Vec<u8>,
    offset: usize,
}

struct Shared {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    sink: Mutex<Option<Arc<dyn ClipboardSink>>>,
    offer: Mutex<Option<Offer>>,
}

type Offer = (Vec<ClipFormat>, Arc<dyn ClipboardProvider>);

pub struct X11Clipboard {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl X11Clipboard {
    pub fn new() -> Result<Self, PlatformError> {
        let (conn, screen) =
            x11rb::connect(None).map_err(|e| PlatformError::Unsupported(e.to_string()))?;
        let root = conn.setup().roots[screen].root;
        conn.xfixes_query_version(5, 0)
            .map_err(os)?
            .reply()
            .map_err(os)?;
        let window = conn.generate_id().map_err(os)?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(os)?;
        let atoms = Atoms::intern(&conn)?;
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(os)?;
        conn.flush().map_err(os)?;
        Ok(Self {
            shared: Arc::new(Shared {
                conn,
                window,
                atoms,
                sink: Mutex::new(None),
                offer: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        })
    }
}

fn report(shared: &Shared, owner: Window) {
    let sink = shared
        .sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(sink) = sink else { return };
    if owner == shared.window {
        let formats = shared
            .offer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|(f, _)| f.clone())
            .unwrap_or_default();
        sink.changed(ClipChange {
            formats,
            own: true,
            concealed: false,
        });
        return;
    }
    if owner == NONE {
        sink.changed(ClipChange::default());
        return;
    }
    // Asking the owner for TARGETS is a round trip through it; keep the event loop free.
    let _ = std::thread::Builder::new()
        .name("bridge-x11-targets".into())
        .spawn(move || {
            let Ok(reader) = Reader::new() else { return };
            let targets = reader.targets();
            let a = reader.atoms;
            let mut formats: Vec<ClipFormat> =
                targets.iter().filter_map(|t| a.format_of(*t)).collect();
            formats.sort();
            formats.dedup();
            sink.changed(ClipChange {
                formats,
                own: false,
                concealed: targets.contains(&a.kde_password),
            });
        });
}

fn notify(shared: &Shared, req: &SelectionRequestEvent, property: Atom) {
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    let _ = shared
        .conn
        .send_event(false, req.requestor, EventMask::NO_EVENT, ev);
    let _ = shared.conn.flush();
}

fn serve(
    shared: &Shared,
    req: &SelectionRequestEvent,
    outgoing: &mut HashMap<(Window, Atom), Outgoing>,
) {
    let a = shared.atoms;
    let c = &shared.conn;
    let offer = shared
        .offer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let property = if req.property == NONE {
        req.target
    } else {
        req.property
    };
    let Some((formats, provider)) = offer else {
        notify(shared, req, NONE);
        return;
    };
    if req.target == a.targets {
        let mut list = vec![a.targets];
        list.extend(formats.iter().flat_map(|f| a.targets_of(*f)));
        let _ = c.change_property32(
            PropMode::REPLACE,
            req.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        );
        notify(shared, req, property);
        return;
    }
    let data = a
        .format_of(req.target)
        .filter(|f| formats.contains(f))
        .and_then(|f| provider.render(f));
    let Some(data) = data else {
        notify(shared, req, NONE);
        return;
    };
    let chunk = (c.maximum_request_bytes() / 2).clamp(4096, 256 * 1024);
    if data.len() <= chunk {
        let _ = c.change_property8(
            PropMode::REPLACE,
            req.requestor,
            property,
            req.target,
            &data,
        );
        notify(shared, req, property);
        return;
    }
    let _ = c.change_window_attributes(
        req.requestor,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    );
    let _ = c.change_property32(
        PropMode::REPLACE,
        req.requestor,
        property,
        a.incr,
        &[data.len() as u32],
    );
    outgoing.insert(
        (req.requestor, property),
        Outgoing {
            ty: req.target,
            data,
            offset: 0,
        },
    );
    notify(shared, req, property);
}

fn continue_incr(
    shared: &Shared,
    window: Window,
    atom: Atom,
    outgoing: &mut HashMap<(Window, Atom), Outgoing>,
) {
    let Some(o) = outgoing.get_mut(&(window, atom)) else {
        return;
    };
    let chunk = (shared.conn.maximum_request_bytes() / 2).clamp(4096, 256 * 1024);
    let end = (o.offset + chunk).min(o.data.len());
    let _ = shared.conn.change_property8(
        PropMode::REPLACE,
        window,
        atom,
        o.ty,
        &o.data[o.offset..end],
    );
    let _ = shared.conn.flush();
    if o.offset == o.data.len() {
        // The zero-length chunk just written ends the transfer.
        outgoing.remove(&(window, atom));
    } else {
        o.offset = end;
    }
}

fn run(shared: Arc<Shared>) {
    let mut outgoing: HashMap<(Window, Atom), Outgoing> = HashMap::new();
    loop {
        let Ok(event) = shared.conn.wait_for_event() else {
            return;
        };
        match event {
            Event::ClientMessage(m) if m.window == shared.window => return,
            Event::XfixesSelectionNotify(n) if n.selection == shared.atoms.clipboard => {
                report(&shared, n.owner);
            }
            Event::SelectionRequest(req) => serve(&shared, &req, &mut outgoing),
            Event::SelectionClear(c) if c.selection == shared.atoms.clipboard => {
                *shared.offer.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            Event::PropertyNotify(p) if p.state == Property::DELETE => {
                continue_incr(&shared, p.window, p.atom, &mut outgoing);
            }
            _ => {}
        }
    }
}

impl Clipboard for X11Clipboard {
    fn start(&self, sink: Arc<dyn ClipboardSink>) -> Result<(), PlatformError> {
        *self.shared.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            let shared = self.shared.clone();
            *guard = Some(
                std::thread::Builder::new()
                    .name("bridge-x11-clipboard".into())
                    .spawn(move || run(shared))
                    .map_err(os)?,
            );
        }
        Ok(())
    }

    fn read(&self, format: ClipFormat) -> Result<Option<Vec<u8>>, PlatformError> {
        let reader = Reader::new()?;
        let targets = reader.targets();
        let a = reader.atoms;
        let Some(target) = a
            .targets_of(format)
            .into_iter()
            .find(|t| targets.contains(t))
        else {
            return Ok(None);
        };
        let data = reader.convert(target, Duration::from_secs(20));
        Ok(data.map(|d| {
            if format == ClipFormat::HTML {
                decode_html(&d)
            } else {
                d
            }
        }))
    }

    fn announce(
        &self,
        formats: &[ClipFormat],
        provider: Arc<dyn ClipboardProvider>,
    ) -> Result<(), PlatformError> {
        let s = &self.shared;
        *s.offer.lock().unwrap_or_else(|p| p.into_inner()) = Some((formats.to_vec(), provider));
        s.conn
            .set_selection_owner(s.window, s.atoms.clipboard, CURRENT_TIME)
            .map_err(os)?;
        s.conn.flush().map_err(os)?;
        let owner = s
            .conn
            .get_selection_owner(s.atoms.clipboard)
            .map_err(os)?
            .reply()
            .map_err(os)?
            .owner;
        if owner == s.window {
            Ok(())
        } else {
            Err(PlatformError::Os(
                "another client kept the clipboard".into(),
            ))
        }
    }

    fn stop(&self) {
        let s = &self.shared;
        *s.sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
        let wake = ClientMessageEvent::new(32, s.window, 0u32, [0u32; 5]);
        let _ = s
            .conn
            .send_event(false, s.window, EventMask::NO_EVENT, wake);
        let _ = s.conn.flush();
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_html_is_decoded() {
        let mut v = vec![0xFF, 0xFE];
        v.extend("<b>я</b>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_html(&v), "<b>я</b>".as_bytes());
        assert_eq!(decode_html(b"<i>x</i>"), b"<i>x</i>");
    }
}
