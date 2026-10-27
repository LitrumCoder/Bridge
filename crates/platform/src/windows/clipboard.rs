//! Win32 clipboard with delayed rendering (ADR-0007): a message-only window listens for
//! `WM_CLIPBOARDUPDATE`, owns the clipboard when we put a peer's offer there, and
//! renders a format only on `WM_RENDERFORMAT`, when an application pastes it.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, WM_APP, WM_CLIPBOARDUPDATE, WM_CLOSE,
    WM_DESTROY, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSW,
};

use crate::clipfmt::{crlf_to_lf, dib_to_png, from_cf_html, lf_to_crlf, png_to_dib, to_cf_html};
use crate::{ClipChange, ClipFormat, Clipboard, ClipboardProvider, ClipboardSink, PlatformError};

const WM_ANNOUNCE: u32 = WM_APP + 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Formats {
    html: u32,
    rtf: u32,
    png: u32,
    exclude: u32,
    viewer_ignore: u32,
    history: u32,
    cloud: u32,
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| {
        // SAFETY: NUL-terminated UTF-16 names that live across each call.
        let reg = |n: &str| unsafe { RegisterClipboardFormatW(wide(n).as_ptr()) };
        Formats {
            html: reg("HTML Format"),
            rtf: reg("Rich Text Format"),
            png: reg("PNG"),
            exclude: reg("ExcludeClipboardContentFromMonitorProcessing"),
            viewer_ignore: reg("Clipboard Viewer Ignore"),
            history: reg("CanIncludeInClipboardHistory"),
            cloud: reg("CanUploadToCloudClipboard"),
        }
    })
}

fn win_formats(f: ClipFormat) -> Vec<u32> {
    let r = formats();
    match f {
        ClipFormat::TEXT => vec![u32::from(CF_UNICODETEXT)],
        ClipFormat::HTML => vec![r.html],
        ClipFormat::RTF => vec![r.rtf],
        ClipFormat::PNG => vec![r.png, u32::from(CF_DIB)],
        _ => Vec::new(),
    }
}

fn wire_format(win: u32) -> Option<ClipFormat> {
    let r = formats();
    match win {
        w if w == u32::from(CF_UNICODETEXT) => Some(ClipFormat::TEXT),
        w if w == r.html => Some(ClipFormat::HTML),
        w if w == r.rtf => Some(ClipFormat::RTF),
        w if w == r.png || w == u32::from(CF_DIB) => Some(ClipFormat::PNG),
        _ => None,
    }
}

/// Wire bytes → what Windows stores under `win`.
fn to_windows(win: u32, data: &[u8]) -> Option<Vec<u8>> {
    let r = formats();
    match win {
        w if w == u32::from(CF_UNICODETEXT) => {
            let text = lf_to_crlf(&String::from_utf8_lossy(data));
            Some(
                text.encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            )
        }
        w if w == r.html => Some(to_cf_html(&String::from_utf8_lossy(data))),
        w if w == r.rtf || w == r.png => Some(data.to_vec()),
        w if w == u32::from(CF_DIB) => png_to_dib(data),
        _ => None,
    }
}

type Pending = (Vec<ClipFormat>, Arc<dyn ClipboardProvider>, Sender<bool>);

struct State {
    sink: Mutex<Option<Arc<dyn ClipboardSink>>>,
    provider: Mutex<Option<Arc<dyn ClipboardProvider>>>,
    pending: Mutex<Option<Pending>>,
    hwnd: AtomicIsize,
}

fn state() -> &'static State {
    static S: OnceLock<State> = OnceLock::new();
    S.get_or_init(|| State {
        sink: Mutex::new(None),
        provider: Mutex::new(None),
        pending: Mutex::new(None),
        hwnd: AtomicIsize::new(0),
    })
}

fn hwnd() -> HWND {
    state().hwnd.load(Ordering::Acquire) as HWND
}

/// Opens the clipboard, retrying while another application holds it.
fn open(owner: HWND) -> bool {
    // Clipboard monitors hold it for a moment after every change.
    for _ in 0..100 {
        // SAFETY: plain Win32 call.
        if unsafe { OpenClipboard(owner) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn available(win: u32) -> bool {
    // SAFETY: plain Win32 query, no open clipboard needed.
    unsafe { IsClipboardFormatAvailable(win) != 0 }
}

/// Copies a clipboard handle's bytes; the clipboard must be open.
fn handle_bytes(h: HANDLE) -> Option<Vec<u8>> {
    if h.is_null() {
        return None;
    }
    // SAFETY: `h` is a global memory handle owned by the clipboard while it is open;
    // we only read `GlobalSize` bytes from the locked pointer.
    unsafe {
        let size = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h);
        Some(v)
    }
}

fn set_bytes(win: u32, data: &[u8]) -> bool {
    // SAFETY: allocate, fill and hand over a movable global block; on success the
    // clipboard owns it.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
        if h.is_null() {
            return false;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        GlobalUnlock(h);
        !SetClipboardData(win, h).is_null()
    }
}

fn report_change(own_hwnd: HWND) {
    let r = formats();
    // SAFETY: plain Win32 query.
    let own = unsafe { GetClipboardOwner() } == own_hwnd;
    let mut fmts = Vec::new();
    for f in [ClipFormat::TEXT, ClipFormat::HTML, ClipFormat::RTF] {
        if win_formats(f).into_iter().any(available) {
            fmts.push(f);
        }
    }
    if win_formats(ClipFormat::PNG).into_iter().any(available) {
        fmts.push(ClipFormat::PNG);
    }
    let change = ClipChange {
        formats: fmts,
        own,
        concealed: available(r.exclude) || available(r.viewer_ignore),
    };
    let sink = state()
        .sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Some(s) = sink {
        s.changed(change);
    }
}

fn do_announce(own_hwnd: HWND) {
    let Some((fmts, provider, done)) = state()
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    else {
        return;
    };
    let ok = open(own_hwnd) && {
        // SAFETY: the clipboard is open by this window; NULL data means delayed rendering.
        unsafe {
            EmptyClipboard();
            for f in &fmts {
                for w in win_formats(*f) {
                    SetClipboardData(w, std::ptr::null_mut());
                }
            }
            // Clipboard history and monitors would render every format right away,
            // pulling it over the network; a peer's copy stays out of them.
            let r = formats();
            set_bytes(r.history, &0u32.to_le_bytes());
            set_bytes(r.cloud, &0u32.to_le_bytes());
            set_bytes(r.exclude, &[0]);
            CloseClipboard();
        }
        true
    };
    if ok {
        *state().provider.lock().unwrap_or_else(|p| p.into_inner()) = Some(provider);
    }
    let _ = done.send(ok);
}

fn render(win: u32) {
    let Some(f) = wire_format(win) else { return };
    let provider = state()
        .provider
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // Blocks this thread (and the pasting application) until the data arrives.
    if let Some(data) = provider.and_then(|p| p.render(f))
        && let Some(bytes) = to_windows(win, &data)
    {
        set_bytes(win, &bytes);
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => report_change(h),
        WM_ANNOUNCE => do_announce(h),
        WM_RENDERFORMAT => render(wp as u32),
        WM_RENDERALLFORMATS => {
            // Shutting down while a peer's offer is ours: drop it rather than fetch
            // everything over the network now.
            if open(h) {
                // SAFETY: the clipboard is open by this window.
                unsafe {
                    if GetClipboardOwner() == h {
                        EmptyClipboard();
                    }
                    CloseClipboard();
                }
            }
        }
        WM_DESTROYCLIPBOARD => {
            *state().provider.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        WM_CLOSE => {
            // SAFETY: our own window, on its thread.
            unsafe {
                RemoveClipboardFormatListener(h);
                DestroyWindow(h);
            }
        }
        // SAFETY: ends this thread's message loop.
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        // SAFETY: default handling with unchanged arguments.
        _ => return unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
    0
}

fn run(ready: Sender<Result<(), String>>) {
    let class = wide("BridgeClipboardWindow");
    // SAFETY: the class name and window procedure outlive the window; message-only
    // window owned by this thread.
    let h = unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        )
    };
    // SAFETY: `h` is checked before use.
    if h.is_null() || unsafe { AddClipboardFormatListener(h) } == 0 {
        let _ = ready.send(Err("cannot create the clipboard listener window".into()));
        return;
    }
    state().hwnd.store(h as isize, Ordering::Release);
    let _ = ready.send(Ok(()));
    // SAFETY: standard message loop for the window owned by this thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
    state().hwnd.store(0, Ordering::Release);
}

#[derive(Default)]
pub struct WinClipboard {
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WinClipboard {
    pub fn new() -> Result<Self, PlatformError> {
        let _ = formats();
        Ok(Self::default())
    }
}

fn read_windows(format: ClipFormat) -> Option<Vec<u8>> {
    let r = formats();
    let get = |w: u32| -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open on this thread (see below).
        handle_bytes(unsafe { GetClipboardData(w) })
    };
    match format {
        ClipFormat::TEXT => {
            let raw = get(u32::from(CF_UNICODETEXT))?;
            let units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|&u| u != 0)
                .collect();
            Some(crlf_to_lf(&String::from_utf16_lossy(&units)).into_bytes())
        }
        ClipFormat::HTML => from_cf_html(&get(r.html)?).map(String::into_bytes),
        ClipFormat::RTF => {
            let mut v = get(r.rtf)?;
            while v.last() == Some(&0) {
                v.pop();
            }
            Some(v)
        }
        ClipFormat::PNG => {
            if available(r.png)
                && let Some(png) = get(r.png)
            {
                return Some(png);
            }
            dib_to_png(&get(u32::from(CF_DIB))?)
        }
        _ => None,
    }
}

impl Clipboard for WinClipboard {
    fn start(&self, sink: Arc<dyn ClipboardSink>) -> Result<(), PlatformError> {
        *state().sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let (tx, rx) = channel();
        let handle = std::thread::Builder::new()
            .name("bridge-clipboard".into())
            .spawn(move || run(tx))
            .map_err(|e| PlatformError::Os(e.to_string()))?;
        rx.recv()
            .map_err(|_| PlatformError::Os("clipboard thread died".into()))?
            .map_err(PlatformError::Os)?;
        *guard = Some(handle);
        Ok(())
    }

    fn read(&self, format: ClipFormat) -> Result<Option<Vec<u8>>, PlatformError> {
        if !win_formats(format).into_iter().any(available) {
            return Ok(None);
        }
        if !open(std::ptr::null_mut()) {
            return Err(PlatformError::Os("the clipboard is busy".into()));
        }
        let data = read_windows(format);
        // SAFETY: we opened it above.
        unsafe { CloseClipboard() };
        Ok(data)
    }

    fn announce(
        &self,
        formats: &[ClipFormat],
        provider: Arc<dyn ClipboardProvider>,
    ) -> Result<(), PlatformError> {
        let h = hwnd();
        if h.is_null() {
            return Err(PlatformError::Os(
                "the clipboard listener is not running".into(),
            ));
        }
        let (tx, rx) = channel();
        *state().pending.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((formats.to_vec(), provider, tx));
        // SAFETY: posting to our own window.
        unsafe { PostMessageW(h, WM_ANNOUNCE, 0, 0) };
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(true) => Ok(()),
            _ => Err(PlatformError::Os("cannot take the clipboard".into())),
        }
    }

    fn stop(&self) {
        *state().sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
        let h = hwnd();
        if !h.is_null() {
            // SAFETY: posting to our own window.
            unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
        }
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_stored_as_crlf_utf16() {
        let w = to_windows(u32::from(CF_UNICODETEXT), "a\nб".as_bytes()).unwrap();
        let units: Vec<u16> = w
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(units, "a\r\nб\0".encode_utf16().collect::<Vec<_>>());
        assert_eq!(wire_format(formats().png), Some(ClipFormat::PNG));
        assert_eq!(wire_format(u32::from(CF_DIB)), Some(ClipFormat::PNG));
        assert_eq!(win_formats(ClipFormat::FILES), Vec::<u32>::new());
    }
}
