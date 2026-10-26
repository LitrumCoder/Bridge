use std::sync::Mutex;
use std::time::{Duration, Instant};

use bridge_protocol::BoundedString;
use bridge_protocol::input::{LayoutTag, LockState};
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Globalization::LCIDToLocaleName;
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, VK_CAPITAL, VK_NUMLOCK, VK_SCROLL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::{LocalScreen, ScreenInfo};

const CACHE_FOR: Duration = Duration::from_secs(2);

/// Monitors in physical pixels; cached briefly because injection asks for them often.
#[derive(Default)]
pub struct WinScreens {
    cache: Mutex<Option<(Instant, Vec<LocalScreen>)>>,
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: `data` is the `&mut Vec` passed by `enumerate` and outlives the call.
    let out = unsafe { &mut *(data as *mut Vec<(RECT, u32)>) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `info` is a properly sized MONITORINFO.
    if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: out-pointers to locals.
        let ok = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } == 0;
        out.push((info.rcMonitor, if ok { dx } else { 96 }));
    }
    1
}

fn enumerate() -> Vec<LocalScreen> {
    let mut raw: Vec<(RECT, u32)> = Vec::new();
    // SAFETY: the callback only writes into `raw`, which lives across the call.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut raw as *mut _ as LPARAM,
        );
    }
    raw.sort_by_key(|(r, _)| (r.left, r.top));
    raw.into_iter()
        .enumerate()
        .map(|(i, (r, dpi))| LocalScreen {
            id: i as u32,
            px_w: (r.right - r.left).max(1) as u32,
            px_h: (r.bottom - r.top).max(1) as u32,
            origin_x: r.left,
            origin_y: r.top,
            scale_permille: dpi * 1000 / 96,
            // TODO(M4b+): physical size from EDID (registry); until then sizes come from DPI.
            size_mm: None,
        })
        .collect()
}

impl WinScreens {
    pub(crate) fn cached(&self) -> Vec<LocalScreen> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, s)) = cache.as_ref()
            && at.elapsed() < CACHE_FOR
        {
            return s.clone();
        }
        let fresh = enumerate();
        *cache = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

impl ScreenInfo for WinScreens {
    fn screens(&self) -> Vec<LocalScreen> {
        let fresh = enumerate();
        *self.cache.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((Instant::now(), fresh.clone()));
        fresh
    }

    fn layout_tag(&self) -> Option<LayoutTag> {
        // SAFETY: plain Win32 queries about the foreground window.
        let langid = unsafe {
            let hwnd = GetForegroundWindow();
            let tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
            (GetKeyboardLayout(tid) as usize & 0xFFFF) as u32
        };
        let mut buf = [0u16; 85];
        // SAFETY: buffer and its length are passed together.
        let n = unsafe { LCIDToLocaleName(langid, buf.as_mut_ptr(), buf.len() as i32, 0) };
        let lang = if n > 1 {
            String::from_utf16_lossy(&buf[..n as usize - 1])
        } else {
            String::new()
        };
        Some(LayoutTag {
            lang: BoundedString::truncating(&lang),
            id: BoundedString::truncating(&format!("{langid:08X}")),
        })
    }

    fn locks(&self) -> LockState {
        // SAFETY: GetKeyState has no preconditions.
        let on = |vk: u16| unsafe { GetKeyState(i32::from(vk)) } & 1 == 1;
        LockState {
            caps: on(VK_CAPITAL),
            num: on(VK_NUMLOCK),
            scroll: on(VK_SCROLL),
        }
    }
}
