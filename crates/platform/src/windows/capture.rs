//! Low-level hooks on a dedicated thread with its own message loop. Hook procedures
//! must return fast (Windows silently removes hooks slower than `LowLevelHooksTimeout`),
//! so they only ask the sink for a decision and never block; the hooks are re-installed
//! periodically in case the system removed them.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;

use bridge_protocol::input::MouseButton;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetMessageW, GetSystemMetrics, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_UP, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, SM_CXSCREEN, SM_CYSCREEN,
    SetCursorPos, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::INJECTED_MARK;
use crate::keymap::set1_to_hid;
use crate::{CaptureEvent, CaptureSink, Decision, InputCapture, PlatformError};

const REHOOK_MS: u32 = 60_000;

/// State shared with the hook procedures, which receive no user data.
struct HookState {
    sink: RwLock<Option<Arc<dyn CaptureSink>>>,
    grab: AtomicBool,
    last_x: AtomicI32,
    last_y: AtomicI32,
    park_x: AtomicI32,
    park_y: AtomicI32,
    pressed: Mutex<HashSet<u16>>,
}

static STATE: OnceLock<HookState> = OnceLock::new();

fn state() -> &'static HookState {
    STATE.get_or_init(|| HookState {
        sink: RwLock::new(None),
        grab: AtomicBool::new(false),
        last_x: AtomicI32::new(0),
        last_y: AtomicI32::new(0),
        park_x: AtomicI32::new(0),
        park_y: AtomicI32::new(0),
        pressed: Mutex::new(HashSet::new()),
    })
}

fn decide(ev: CaptureEvent) -> Decision {
    let sink = state()
        .sink
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    sink.map_or(Decision::Pass, |s| s.on_event(ev))
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        // SAFETY: forwarding the unchanged arguments.
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    // SAFETY: for WH_MOUSE_LL, lparam points to an MSLLHOOKSTRUCT.
    let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    if info.dwExtraInfo == INJECTED_MARK {
        // SAFETY: as above.
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let s = state();
    let msg = wparam as u32;
    let high = (info.mouseData >> 16) as u16;
    let ev = match msg {
        WM_MOUSEMOVE => {
            let grabbing = s.grab.load(Ordering::Acquire);
            // Swallowed moves never move the cursor, so in grab mode it stays parked
            // and `pt - park` is exactly the movement.
            let (x, y) = if grabbing {
                (
                    s.park_x.load(Ordering::Relaxed),
                    s.park_y.load(Ordering::Relaxed),
                )
            } else {
                (
                    s.last_x.load(Ordering::Relaxed),
                    s.last_y.load(Ordering::Relaxed),
                )
            };
            Some(CaptureEvent::Motion {
                x,
                y,
                dx: info.pt.x - x,
                dy: info.pt.y - y,
            })
        }
        WM_LBUTTONDOWN => Some(CaptureEvent::Button {
            button: MouseButton::Left,
            down: true,
        }),
        WM_LBUTTONUP => Some(CaptureEvent::Button {
            button: MouseButton::Left,
            down: false,
        }),
        WM_RBUTTONDOWN => Some(CaptureEvent::Button {
            button: MouseButton::Right,
            down: true,
        }),
        WM_RBUTTONUP => Some(CaptureEvent::Button {
            button: MouseButton::Right,
            down: false,
        }),
        WM_MBUTTONDOWN => Some(CaptureEvent::Button {
            button: MouseButton::Middle,
            down: true,
        }),
        WM_MBUTTONUP => Some(CaptureEvent::Button {
            button: MouseButton::Middle,
            down: false,
        }),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = if high == 1 {
                MouseButton::Back
            } else {
                MouseButton::Forward
            };
            Some(CaptureEvent::Button {
                button,
                down: msg == WM_XBUTTONDOWN,
            })
        }
        WM_MOUSEWHEEL => Some(CaptureEvent::Wheel {
            dx: 0,
            dy: i32::from(high as i16),
        }),
        WM_MOUSEHWHEEL => Some(CaptureEvent::Wheel {
            dx: i32::from(high as i16),
            dy: 0,
        }),
        _ => None,
    };
    let decision = ev.map_or(Decision::Pass, decide);
    if msg == WM_MOUSEMOVE && decision == Decision::Pass && !s.grab.load(Ordering::Acquire) {
        s.last_x.store(info.pt.x, Ordering::Relaxed);
        s.last_y.store(info.pt.y, Ordering::Relaxed);
    }
    match decision {
        Decision::Swallow => 1,
        // SAFETY: forwarding the unchanged arguments.
        Decision::Pass => unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) },
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        // SAFETY: forwarding the unchanged arguments.
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    // SAFETY: for WH_KEYBOARD_LL, lparam points to a KBDLLHOOKSTRUCT.
    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let msg = wparam as u32;
    if info.dwExtraInfo == INJECTED_MARK
        || !matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
    {
        // SAFETY: as above.
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let mut scan = info.scanCode as u16;
    if scan == 0 {
        // Some software injects keys with a virtual key only.
        // SAFETY: MapVirtualKeyW has no preconditions.
        scan = unsafe { MapVirtualKeyW(info.vkCode, MAPVK_VK_TO_VSC_EX) } as u16;
    } else if info.flags & LLKHF_EXTENDED != 0 {
        scan |= 0xE000;
    }
    if scan == 0xE045 {
        // NumLock reports the extended flag; its set 1 code is plain 0x45.
        scan = 0x45;
    }
    let down = info.flags & LLKHF_UP == 0;
    let decision = match set1_to_hid(scan) {
        None => Decision::Pass,
        Some(usage) => {
            let repeat = {
                let mut pressed = state().pressed.lock().unwrap_or_else(|p| p.into_inner());
                if down {
                    !pressed.insert(scan)
                } else {
                    pressed.remove(&scan);
                    false
                }
            };
            decide(CaptureEvent::Key {
                usage,
                down,
                repeat,
            })
        }
    };
    match decision {
        Decision::Swallow => 1,
        // SAFETY: forwarding the unchanged arguments.
        Decision::Pass => unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) },
    }
}

fn install() -> Option<(HHOOK, HHOOK)> {
    // SAFETY: hook procedures are 'static functions; the module handle is our own.
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        if m.is_null() || k.is_null() {
            if !m.is_null() {
                UnhookWindowsHookEx(m);
            }
            if !k.is_null() {
                UnhookWindowsHookEx(k);
            }
            return None;
        }
        Some((m, k))
    }
}

fn uninstall(hooks: (HHOOK, HHOOK)) {
    // SAFETY: handles came from SetWindowsHookExW on this thread.
    unsafe {
        UnhookWindowsHookEx(hooks.0);
        UnhookWindowsHookEx(hooks.1);
    }
}

fn cursor() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: out-pointer to a local.
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

#[derive(Default)]
pub struct WinCapture {
    thread: Mutex<Option<(JoinHandle<()>, Arc<AtomicU32>)>>,
}

impl WinCapture {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputCapture for WinCapture {
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError> {
        let s = state();
        *s.sink.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let (x, y) = cursor();
        s.last_x.store(x, Ordering::Relaxed);
        s.last_y.store(y, Ordering::Relaxed);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let tid = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let tid2 = tid.clone();
        let handle = std::thread::Builder::new()
            .name("bridge-input-hooks".into())
            .spawn(move || {
                // SAFETY: current thread id, no preconditions.
                tid2.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
                let Some(mut hooks) = install() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                // SAFETY: a thread timer without a window; messages arrive in this loop.
                unsafe { SetTimer(std::ptr::null_mut(), 0, REHOOK_MS, None) };
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                // SAFETY: standard message loop on the thread that owns the hooks.
                while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                    if msg.message == WM_TIMER {
                        uninstall(hooks);
                        match install() {
                            Some(h) => hooks = h,
                            None => return,
                        }
                    }
                }
                uninstall(hooks);
            })
            .map_err(|e| PlatformError::Os(e.to_string()))?;
        if !ready_rx.recv().unwrap_or(false) {
            return Err(PlatformError::Os(
                "cannot install low-level input hooks".into(),
            ));
        }
        *guard = Some((handle, tid));
        Ok(())
    }

    fn set_grab(&self, grab: bool) {
        let s = state();
        if grab {
            // SAFETY: GetSystemMetrics has no preconditions.
            let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            let (px, py) = (w / 2, h / 2);
            s.park_x.store(px, Ordering::Relaxed);
            s.park_y.store(py, Ordering::Relaxed);
            // SAFETY: plain cursor move.
            unsafe { SetCursorPos(px, py) };
        }
        s.grab.store(grab, Ordering::Release);
    }

    fn warp(&self, x: i32, y: i32) {
        let s = state();
        // SAFETY: plain cursor move.
        unsafe { SetCursorPos(x, y) };
        s.last_x.store(x, Ordering::Relaxed);
        s.last_y.store(y, Ordering::Relaxed);
    }

    fn stop(&self) {
        *state().sink.write().unwrap_or_else(|p| p.into_inner()) = None;
        state().grab.store(false, Ordering::Release);
        if let Some((handle, tid)) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // SAFETY: posting WM_QUIT to our own hook thread.
            unsafe { PostThreadMessageW(tid.load(Ordering::Acquire), WM_QUIT, 0, 0) };
            let _ = handle.join();
        }
    }
}
