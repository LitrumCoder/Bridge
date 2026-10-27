//! OLE drag and drop (ADR-0008).
//!
//! Source side: thin, almost transparent topmost strips along the outer edges of the
//! virtual desktop are OLE drop targets. They are shown only while the left button
//! is down, so clicks at the very edge still reach applications. A drag with files
//! that touches a strip reports its paths; once it crossed to another device, a
//! window covering the whole desktop keeps the local drag loop over Bridge, and the
//! local drop does nothing.
//!
//! Target side: `DoDragDrop` with `CF_HDROP` listing the staged files, driven by the
//! injected cursor and button. The drop target reads the paths when it drops;
//! `GetData` waits until the transfer is complete.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_FAIL,
    E_NOTIMPL, HGLOBAL, HWND, LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED, POINTL, S_OK, WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Com::{
    DATADIR_GET, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
    IDropSource_Impl, IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize,
    RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP, SHCreateStdEnumFmtEtc};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, HWND_TOPMOST, KillTimer, LWA_ALPHA, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    ShowWindow, WM_APP, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{BOOL, HRESULT, PCWSTR, Ref, implement, w};

use crate::{DragSink, DragSource, DropItem, DropProgress, DropSink, DropTarget, PlatformError};

const STRIP: i32 = 2;
const WM_HOLD: u32 = WM_APP + 10;
const WM_FINISH: u32 = WM_APP + 11;
const POLL_MS: u32 = 50;

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: 1, // DVASPECT_CONTENT
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// `DROPFILES` followed by NUL-separated wide paths and a final NUL.
pub(crate) fn dropfiles_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let header = std::mem::size_of::<DROPFILES>();
    let mut out = vec![0u8; header];
    out[..4].copy_from_slice(&(header as u32).to_le_bytes());
    // fWide (the last BOOL of the header) = TRUE.
    out[header - 4..].copy_from_slice(&1u32.to_le_bytes());
    for p in paths {
        let wide: Vec<u16> = p
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        out.extend(wide.iter().flat_map(|u| u.to_le_bytes()));
    }
    out.extend_from_slice(&[0, 0]);
    out
}

fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: a fresh movable block of the right size, filled while locked.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

fn paths_of(data: &IDataObject) -> Option<Vec<PathBuf>> {
    let fmt = hdrop_format();
    // SAFETY: standard IDataObject call; the medium is released below.
    let mut medium = unsafe { data.GetData(&fmt) }.ok()?;
    // SAFETY: TYMED_HGLOBAL was requested, so `hGlobal` is the active member.
    let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
    // SAFETY: querying a valid HDROP; buffers are sized from the first call.
    let paths = unsafe {
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        (0..n)
            .map(|i| {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(hdrop, i, Some(&mut buf));
                PathBuf::from(String::from_utf16_lossy(&buf[..len]))
            })
            .collect()
    };
    // SAFETY: the medium came from GetData.
    unsafe { ReleaseStgMedium(&mut medium) };
    Some(paths)
}

#[derive(Default)]
struct EdgeShared {
    sink: Mutex<Option<Arc<dyn DragSink>>>,
    holding: AtomicBool,
    inside: AtomicBool,
}

#[implement(IDropTarget)]
struct EdgeTarget(Arc<EdgeShared>);

impl IDropTarget_Impl for EdgeTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = data.as_ref().and_then(paths_of).unwrap_or_default();
        let s = &self.0;
        let ok = !paths.is_empty();
        if ok && !s.holding.load(Ordering::Acquire) {
            s.inside.store(true, Ordering::Release);
            if let Some(sink) = s.sink.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                sink.drag_entered(paths);
            }
        }
        // SAFETY: OLE passes a valid out-pointer.
        unsafe { *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let s = &self.0;
        let on = s.inside.load(Ordering::Acquire) || s.holding.load(Ordering::Acquire);
        // SAFETY: OLE passes a valid out-pointer.
        unsafe { *effect = if on { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let s = &self.0;
        if !s.holding.load(Ordering::Acquire)
            && s.inside.swap(false, Ordering::AcqRel)
            && let Some(sink) = s.sink.lock().unwrap_or_else(|p| p.into_inner()).clone()
        {
            sink.drag_left();
        }
        Ok(())
    }

    fn Drop(
        &self,
        _data: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // The real drop happens on the other device; nothing moves here.
        self.0.inside.store(false, Ordering::Release);
        // SAFETY: OLE passes a valid out-pointer.
        unsafe { *effect = DROPEFFECT_NONE };
        Ok(())
    }
}

/// Edge strips (left, right, top, bottom) and the full-desktop hold window.
struct Windows {
    strips: [HWND; 4],
    hold: HWND,
}

fn virtual_desktop() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn place(w: &Windows) {
    let (x, y, cx, cy) = virtual_desktop();
    let rects = [
        (x, y, STRIP, cy),
        (x + cx - STRIP, y, STRIP, cy),
        (x, y, cx, STRIP),
        (x, y + cy - STRIP, cx, STRIP),
    ];
    for (h, (rx, ry, rw, rh)) in w.strips.iter().zip(rects) {
        // SAFETY: our own windows.
        let _ = unsafe { SetWindowPos(*h, Some(HWND_TOPMOST), rx, ry, rw, rh, SWP_NOACTIVATE) };
    }
    // SAFETY: as above.
    let _ = unsafe { SetWindowPos(w.hold, Some(HWND_TOPMOST), x, y, cx, cy, SWP_NOACTIVATE) };
}

fn show(h: HWND, on: bool) {
    // SAFETY: our own window.
    let _ = unsafe { ShowWindow(h, if on { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
}

static SOURCE: OnceLock<(Arc<EdgeShared>, AtomicIsize)> = OnceLock::new();

fn source_state() -> &'static (Arc<EdgeShared>, AtomicIsize) {
    SOURCE.get_or_init(|| (Arc::new(EdgeShared::default()), AtomicIsize::new(0)))
}

static WINDOWS: Mutex<Option<(usize, [usize; 4])>> = Mutex::new(None);

fn button_down() -> bool {
    // SAFETY: plain query.
    (unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) } as u16 & 0x8000) != 0
}

unsafe extern "system" fn edge_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ws = *WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    let Some((hold, strips)) = ws else {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(h, msg, wp, lp) };
    };
    let w = Windows {
        strips: strips.map(|s| HWND(s as *mut _)),
        hold: HWND(hold as *mut _),
    };
    let shared = &source_state().0;
    match msg {
        WM_TIMER => {
            let holding = shared.holding.load(Ordering::Acquire);
            let on = button_down() && !holding;
            for s in w.strips {
                show(s, on);
            }
        }
        WM_HOLD => {
            shared.holding.store(true, Ordering::Release);
            place(&w);
            show(w.hold, true);
        }
        WM_FINISH => {
            shared.holding.store(false, Ordering::Release);
            shared.inside.store(false, Ordering::Release);
            show(w.hold, false);
        }
        WM_CLOSE => {
            // SAFETY: our own windows on this thread.
            unsafe {
                let _ = KillTimer(Some(h), 1);
                for s in w.strips.iter().chain([&w.hold]) {
                    let _ = RevokeDragDrop(*s);
                    let _ = DestroyWindow(*s);
                }
                PostQuitMessage(0);
            }
        }
        WM_DESTROY => {}
        // SAFETY: default handling with unchanged arguments.
        _ => return unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
    LRESULT(0)
}

fn run_edges(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    // SAFETY: OLE on this STA thread for its lifetime.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(e.to_string()));
        return;
    }
    let class = w!("BridgeDragEdge");
    let target: IDropTarget = EdgeTarget(source_state().0.clone()).into();
    // SAFETY: class and windows are owned by this thread; the target stays alive in
    // `target` until the windows are revoked.
    let created = unsafe {
        let module = GetModuleHandleW(None).ok();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(edge_proc),
            hInstance: module.map(Into::into).unwrap_or_default(),
            lpszClassName: class,
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let make = || {
            let h = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                module.map(Into::into),
                None,
            )?;
            // Alpha 1 of 255: invisible, yet hit-tested (fully transparent is not).
            SetLayeredWindowAttributes(h, Default::default(), 1, LWA_ALPHA)?;
            RegisterDragDrop(h, &target)?;
            windows::core::Result::Ok(h)
        };
        (|| -> windows::core::Result<Windows> {
            Ok(Windows {
                strips: [make()?, make()?, make()?, make()?],
                hold: make()?,
            })
        })()
    };
    let w = match created {
        Ok(w) => w,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    *WINDOWS.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((w.hold.0 as usize, w.strips.map(|s| s.0 as usize)));
    place(&w);
    source_state()
        .1
        .store(w.strips[0].0 as isize, Ordering::Release);
    // SAFETY: a timer on our own window.
    unsafe { SetTimer(Some(w.strips[0]), 1, POLL_MS, None) };
    let _ = ready.send(Ok(()));
    // SAFETY: standard message loop for windows owned by this thread.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    *WINDOWS.lock().unwrap_or_else(|p| p.into_inner()) = None;
    source_state().1.store(0, Ordering::Release);
    drop(target);
    // SAFETY: balances OleInitialize above.
    unsafe { OleUninitialize() };
}

fn post(msg: u32) {
    let h = source_state().1.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: posting to our own window.
        let _ = unsafe { PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0)) };
    }
}

#[derive(Default)]
pub struct WinDragSource {
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WinDragSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DragSource for WinDragSource {
    fn start(&self, sink: Arc<dyn DragSink>) -> Result<(), PlatformError> {
        *source_state()
            .0
            .sink
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::Builder::new()
            .name("bridge-drag-edges".into())
            .spawn(move || run_edges(tx))
            .map_err(|e| PlatformError::Os(e.to_string()))?;
        rx.recv()
            .map_err(|_| PlatformError::Os("drag thread died".into()))?
            .map_err(PlatformError::Os)?;
        *guard = Some(t);
        Ok(())
    }

    fn hold(&self) {
        post(WM_HOLD);
    }

    fn finish(&self) {
        post(WM_FINISH);
    }

    fn stop(&self) {
        *source_state()
            .0
            .sink
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        post(WM_CLOSE);
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
    }
}

#[implement(IDataObject)]
struct StagedFiles {
    paths: Vec<PathBuf>,
    progress: Arc<dyn DropProgress>,
}

fn is_hdrop(f: *const FORMATETC) -> bool {
    // SAFETY: OLE passes a valid FORMATETC.
    let f = unsafe { &*f };
    f.cfFormat == CF_HDROP.0 && f.tymed & TYMED_HGLOBAL.0 as u32 != 0
}

impl IDataObject_Impl for StagedFiles_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if !is_hdrop(format) {
            return Err(DV_E_FORMATETC.into());
        }
        // The target reads the files right after the drop: they must be complete.
        if !self.progress.wait_complete(Duration::from_secs(6 * 3600)) {
            return Err(E_FAIL.into());
        }
        let h = hglobal_from(&dropfiles_bytes(&self.paths))?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _f: *const FORMATETC, _m: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        if is_hdrop(format) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _in: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        // SAFETY: OLE passes a valid out-pointer.
        unsafe { (*out).ptd = std::ptr::null_mut() };
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _f: *const FORMATETC,
        _m: *const STGMEDIUM,
        _release: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: one valid FORMATETC.
        unsafe { SHCreateStdEnumFmtEtc(&[hdrop_format()]) }
    }

    fn DAdvise(
        &self,
        _f: *const FORMATETC,
        _advf: u32,
        _sink: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _c: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[implement(IDropSource)]
struct InjectedDrag;

impl IDropSource_Impl for InjectedDrag_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn run_drag(items: Vec<DropItem>, progress: Arc<dyn DropProgress>, sink: Arc<dyn DropSink>) {
    // SAFETY: OLE on this thread for the duration of the drag.
    if unsafe { OleInitialize(None) }.is_err() {
        sink.dropped(false);
        return;
    }
    // The button press comes through the input channel and may arrive a moment after
    // the drag offer; DoDragDrop ends at once if the button is up.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !button_down() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let data: IDataObject = StagedFiles {
        paths: items.into_iter().map(|i| i.path).collect(),
        progress,
    }
    .into();
    let source: IDropSource = InjectedDrag.into();
    let mut effect = DROPEFFECT_NONE;
    // SAFETY: both objects are alive for the call; `effect` is a local.
    let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    sink.dropped(hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE);
    drop((data, source));
    // SAFETY: balances OleInitialize above.
    unsafe { OleUninitialize() };
}

#[derive(Default)]
pub struct WinDropTarget;

impl WinDropTarget {
    pub fn new() -> Self {
        Self
    }
}

impl DropTarget for WinDropTarget {
    fn begin(
        &self,
        items: Vec<DropItem>,
        progress: Arc<dyn DropProgress>,
        sink: Arc<dyn DropSink>,
    ) -> Result<(), PlatformError> {
        std::thread::Builder::new()
            .name("bridge-drop".into())
            .spawn(move || run_drag(items, progress, sink))
            .map(|_| ())
            .map_err(|e| PlatformError::Os(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropfiles_layout() {
        let b = dropfiles_bytes(&[PathBuf::from(r"C:\a"), PathBuf::from(r"D:\б")]);
        let header = std::mem::size_of::<DROPFILES>();
        assert_eq!(header, 20);
        assert_eq!(u32::from_le_bytes(b[..4].try_into().unwrap()), 20);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 1, "wide");
        let units: Vec<u16> = b[header..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let expect: Vec<u16> = "C:\\a\0D:\\б\0\0".encode_utf16().collect();
        assert_eq!(units, expect);
    }
}
