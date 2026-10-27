//! XDND (ADR-0008), both roles over plain X protocol.
//!
//! `XdndSource` ("a drag leaves this device") is an XDND *target*: input-only strips at
//! the screen edges, mapped only while button 1 is down, that read `text/uri-list`
//! from the dragging client. Once the drag crossed, a full-screen input-only window
//! takes the rest of the local drag and refuses the drop.
//!
//! `XdndTarget` ("a drag arrives here") is an XDND *source* driven by the injected
//! pointer: it follows the window under the cursor, speaks the protocol to it and
//! serves `text/uri-list` of the staged files once they are complete.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, KeyButMask, PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, StackMode,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use crate::{DragSink, DragSource, DropItem, DropProgress, DropSink, DropTarget, PlatformError};

const XDND_VERSION: u32 = 5;
const STRIP: u16 = 2;

fn os(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Os(e.to_string())
}

/// `file://` URI of an absolute path, percent-encoding everything but unreserved bytes.
pub(crate) fn file_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            let path = rest.strip_prefix("localhost").unwrap_or(rest);
            if !path.starts_with('/') {
                return None;
            }
            let bytes = path.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%'
                    && i + 2 < bytes.len()
                    && let Ok(v) =
                        u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
                {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).ok().map(PathBuf::from)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Atoms {
    aware: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    action_copy: Atom,
    uri_list: Atom,
    transfer: Atom,
}

impl Atoms {
    fn intern(c: &RustConnection) -> Result<Self, PlatformError> {
        let a = |n: &str| -> Result<Atom, PlatformError> {
            Ok(c.intern_atom(false, n.as_bytes())
                .map_err(os)?
                .reply()
                .map_err(os)?
                .atom)
        };
        Ok(Self {
            aware: a("XdndAware")?,
            enter: a("XdndEnter")?,
            position: a("XdndPosition")?,
            status: a("XdndStatus")?,
            leave: a("XdndLeave")?,
            drop: a("XdndDrop")?,
            finished: a("XdndFinished")?,
            selection: a("XdndSelection")?,
            action_copy: a("XdndActionCopy")?,
            uri_list: a("text/uri-list")?,
            transfer: a("BRIDGE_XDND")?,
        })
    }
}

fn connect() -> Result<(RustConnection, Window, Atoms), PlatformError> {
    let (conn, screen) =
        x11rb::connect(None).map_err(|e| PlatformError::Unsupported(e.to_string()))?;
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms::intern(&conn)?;
    Ok((conn, root, atoms))
}

fn send_client(c: &RustConnection, to: Window, ty: Atom, data: [u32; 5]) {
    let ev = ClientMessageEvent::new(32, to, ty, data);
    let _ = c.send_event(false, to, EventMask::NO_EVENT, ev);
    let _ = c.flush();
}

fn input_window(c: &RustConnection, root: Window, a: &Atoms) -> Result<Window, PlatformError> {
    let w = c.generate_id().map_err(os)?;
    c.create_window(
        0,
        w,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(os)?;
    c.change_property32(
        PropMode::REPLACE,
        w,
        a.aware,
        AtomEnum::ATOM,
        &[XDND_VERSION],
    )
    .map_err(os)?;
    Ok(w)
}

fn button1(c: &RustConnection, root: Window) -> Option<(bool, i16, i16)> {
    let r = c.query_pointer(root).ok()?.reply().ok()?;
    Some((r.mask.contains(KeyButMask::BUTTON1), r.root_x, r.root_y))
}

// ---- drag leaves this device: we are the XDND target at the edges ----

struct EdgeState {
    sink: Mutex<Option<Arc<dyn DragSink>>>,
    holding: AtomicBool,
    hold_req: Mutex<Option<bool>>,
    stop: AtomicBool,
}

pub struct XdndSource {
    state: Arc<EdgeState>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl XdndSource {
    pub fn new() -> Result<Self, PlatformError> {
        // Fail early without a display, like the other X11 services.
        connect()?;
        Ok(Self {
            state: Arc::new(EdgeState {
                sink: Mutex::new(None),
                holding: AtomicBool::new(false),
                hold_req: Mutex::new(None),
                stop: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
        })
    }
}

fn run_edges(state: Arc<EdgeState>) -> Result<(), PlatformError> {
    let (c, root, a) = connect()?;
    let geo = c.get_geometry(root).map_err(os)?.reply().map_err(os)?;
    let (w, h) = (geo.width, geo.height);
    let rects = [
        (0i16, 0i16, STRIP, h),
        (w.saturating_sub(STRIP) as i16, 0, STRIP, h),
        (0, 0, w, STRIP),
        (0, h.saturating_sub(STRIP) as i16, w, STRIP),
    ];
    let mut strips = Vec::new();
    for (x, y, sw, sh) in rects {
        let s = input_window(&c, root, &a)?;
        c.configure_window(
            s,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(sw))
                .height(u32::from(sh)),
        )
        .map_err(os)?;
        strips.push(s);
    }
    let hold = input_window(&c, root, &a)?;
    c.configure_window(
        hold,
        &ConfigureWindowAux::new()
            .x(0)
            .y(0)
            .width(u32::from(w))
            .height(u32::from(h)),
    )
    .map_err(os)?;
    c.flush().map_err(os)?;
    let mut mapped = false;
    let mut entered: Option<Window> = None;
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let sink = || state.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
    while !state.stop.load(Ordering::Acquire) {
        if let Some(on) = state
            .hold_req
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            state.holding.store(on, Ordering::Release);
            if on {
                let _ = c.map_window(hold);
                let _ = c.configure_window(
                    hold,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                );
            } else {
                let _ = c.unmap_window(hold);
                entered = None;
            }
            let _ = c.flush();
        }
        if last_poll.elapsed() >= Duration::from_millis(50) {
            last_poll = Instant::now();
            let down = button1(&c, root).is_some_and(|b| b.0);
            let want = down && !state.holding.load(Ordering::Acquire);
            if want != mapped {
                for s in &strips {
                    if want {
                        let _ = c.map_window(*s);
                        let _ = c.configure_window(
                            *s,
                            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                        );
                    } else {
                        let _ = c.unmap_window(*s);
                    }
                }
                let _ = c.flush();
                mapped = want;
            }
        }
        let ev = match c.poll_for_event() {
            Ok(Some(e)) => e,
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => break,
        };
        match ev {
            Event::ClientMessage(m) if m.type_ == a.enter => {
                let d = m.data.as_data32();
                entered = Some(d[0]);
                if !state.holding.load(Ordering::Acquire) {
                    // Ask for the file list; the reply arrives as SelectionNotify.
                    let _ = c.convert_selection(
                        m.window,
                        a.selection,
                        a.uri_list,
                        a.transfer,
                        CURRENT_TIME,
                    );
                    let _ = c.flush();
                }
            }
            Event::ClientMessage(m) if m.type_ == a.position => {
                let src = m.data.as_data32()[0];
                send_client(&c, src, a.status, [m.window, 1, 0, 0, a.action_copy]);
            }
            Event::ClientMessage(m) if m.type_ == a.leave => {
                if entered.take().is_some()
                    && !state.holding.load(Ordering::Acquire)
                    && let Some(s) = sink()
                {
                    s.drag_left();
                }
            }
            Event::ClientMessage(m) if m.type_ == a.drop => {
                // The real drop happens on the other device: refuse it here.
                let src = m.data.as_data32()[0];
                send_client(&c, src, a.finished, [m.window, 0, NONE, 0, 0]);
                entered = None;
            }
            Event::SelectionNotify(n) if n.property != NONE => {
                let value = c
                    .get_property(true, n.requestor, a.transfer, AtomEnum::ANY, 0, 1 << 20)
                    .ok()
                    .and_then(|r| r.reply().ok())
                    .map(|r| r.value)
                    .unwrap_or_default();
                let paths = parse_uri_list(&String::from_utf8_lossy(&value));
                if !paths.is_empty()
                    && let Some(s) = sink()
                {
                    s.drag_entered(paths);
                }
            }
            _ => {}
        }
    }
    for s in strips.iter().chain([&hold]) {
        let _ = c.destroy_window(*s);
    }
    let _ = c.flush();
    Ok(())
}

impl DragSource for XdndSource {
    fn start(&self, sink: Arc<dyn DragSink>) -> Result<(), PlatformError> {
        *self.state.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            self.state.stop.store(false, Ordering::Release);
            let state = self.state.clone();
            *guard = Some(
                std::thread::Builder::new()
                    .name("bridge-xdnd-edges".into())
                    .spawn(move || {
                        if let Err(e) = run_edges(state) {
                            tracing::warn!("XDND edges stopped: {e}");
                        }
                    })
                    .map_err(os)?,
            );
        }
        Ok(())
    }

    fn hold(&self) {
        *self
            .state
            .hold_req
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(true);
    }

    fn finish(&self) {
        *self
            .state
            .hold_req
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(false);
    }

    fn stop(&self) {
        *self.state.sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.state.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
    }
}

// ---- drag arrives here: we are the XDND source under the injected pointer ----

/// The deepest XdndAware window under the pointer, with its protocol version.
fn aware_under(
    c: &RustConnection,
    root: Window,
    a: &Atoms,
    x: i16,
    y: i16,
    skip: Window,
) -> Option<(Window, u32)> {
    let mut found = None;
    let mut w = root;
    for _ in 0..32 {
        let child = c
            .translate_coordinates(root, w, x, y)
            .ok()?
            .reply()
            .ok()?
            .child;
        if child == NONE || child == skip {
            break;
        }
        if let Some(v) = c
            .get_property(false, child, a.aware, AtomEnum::ATOM, 0, 1)
            .ok()
            .and_then(|r| r.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
        {
            found = Some((child, v));
        }
        w = child;
    }
    found
}

fn run_drop(items: Vec<DropItem>, progress: Arc<dyn DropProgress>) -> Result<bool, PlatformError> {
    let (c, root, a) = connect()?;
    let me = c.generate_id().map_err(os)?;
    c.create_window(
        0,
        me,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(os)?;
    c.set_selection_owner(me, a.selection, CURRENT_TIME)
        .map_err(os)?;
    c.flush().map_err(os)?;
    let list: String = items.iter().map(|i| file_uri(&i.path) + "\r\n").collect();

    // The injected press may arrive just after the drag offer.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !button1(&c, root).is_some_and(|b| b.0) {
        if Instant::now() > deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut target: Option<(Window, u32)> = None;
    let mut accepted = false;
    let mut dropped_at: Option<Instant> = None;
    loop {
        while let Ok(Some(ev)) = c.poll_for_event() {
            match ev {
                Event::ClientMessage(m) if m.type_ == a.status => {
                    accepted = m.data.as_data32()[1] & 1 == 1;
                }
                Event::ClientMessage(m) if m.type_ == a.finished => {
                    let d = m.data.as_data32();
                    let ok = target.is_some_and(|(_, v)| v < 5) || d[1] & 1 == 1;
                    return Ok(ok);
                }
                Event::SelectionRequest(req) => {
                    let mut prop = NONE;
                    if req.target == a.uri_list
                        && progress.wait_complete(Duration::from_secs(6 * 3600))
                    {
                        let p = if req.property == NONE {
                            req.target
                        } else {
                            req.property
                        };
                        if c.change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            p,
                            a.uri_list,
                            list.as_bytes(),
                        )
                        .is_ok()
                        {
                            prop = p;
                        }
                    }
                    let n = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: req.time,
                        requestor: req.requestor,
                        selection: req.selection,
                        target: req.target,
                        property: prop,
                    };
                    let _ = c.send_event(false, req.requestor, EventMask::NO_EVENT, n);
                    let _ = c.flush();
                }
                _ => {}
            }
        }
        if let Some(t) = dropped_at {
            if t.elapsed() > Duration::from_secs(30) {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let Some((down, x, y)) = button1(&c, root) else {
            return Ok(false);
        };
        let under = aware_under(&c, root, &a, x, y, me);
        if under.map(|u| u.0) != target.map(|t| t.0) {
            if let Some((old, _)) = target {
                send_client(&c, old, a.leave, [me, 0, 0, 0, 0]);
            }
            accepted = false;
            if let Some((w, v)) = under {
                let version = v.min(XDND_VERSION);
                send_client(&c, w, a.enter, [me, version << 24, a.uri_list, 0, 0]);
            }
            target = under;
        }
        if !down {
            match target {
                Some((w, _)) if accepted => {
                    send_client(&c, w, a.drop, [me, 0, CURRENT_TIME, 0, 0]);
                    dropped_at = Some(Instant::now());
                    continue;
                }
                Some((w, _)) => {
                    send_client(&c, w, a.leave, [me, 0, 0, 0, 0]);
                    return Ok(false);
                }
                None => return Ok(false),
            }
        }
        if let Some((w, _)) = target {
            let pos = ((x as u32) << 16) | (y as u16 as u32);
            send_client(&c, w, a.position, [me, 0, pos, CURRENT_TIME, a.action_copy]);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[derive(Default)]
pub struct XdndTarget;

impl XdndTarget {
    pub fn new() -> Result<Self, PlatformError> {
        connect()?;
        Ok(Self)
    }
}

impl DropTarget for XdndTarget {
    fn begin(
        &self,
        items: Vec<DropItem>,
        progress: Arc<dyn DropProgress>,
        sink: Arc<dyn DropSink>,
    ) -> Result<(), PlatformError> {
        std::thread::Builder::new()
            .name("bridge-xdnd-drop".into())
            .spawn(move || {
                let ok = run_drop(items, progress).unwrap_or(false);
                sink.dropped(ok);
            })
            .map(|_| ())
            .map_err(os)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let p = PathBuf::from("/home/я/Сэмплы/kick 01#.wav");
        let uri = file_uri(&p);
        assert!(uri.starts_with("file:///home/%D1%8F/"));
        assert!(uri.contains("kick%2001%23.wav"));
        assert_eq!(
            parse_uri_list(&format!("# comment\r\n{uri}\r\nhttp://x/y\r\n")),
            vec![p]
        );
        assert_eq!(
            parse_uri_list("file://localhost/tmp/a"),
            vec![PathBuf::from("/tmp/a")]
        );
        assert!(parse_uri_list("file://host/relative").is_empty());
    }
}
