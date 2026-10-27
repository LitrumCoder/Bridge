//! Windows: low-level hooks for capture, `SendInput` for injection (ADR-0005).

mod capture;
mod clipboard;
mod dnd;
mod inject;
mod midi;
mod network;
mod screens;

use std::sync::Arc;

pub use capture::WinCapture;
pub use clipboard::WinClipboard;
pub use dnd::{WinDragSource, WinDropTarget};
pub use inject::WinInjector;
pub use midi::WinMidi;
pub(crate) use network::current_network;
pub use screens::WinScreens;

use crate::{Platform, PlatformError};

/// Marks events we inject so our own hooks ignore them (no feedback loops).
pub(crate) const INJECTED_MARK: usize = 0x4252_4447; // "BRDG"

/// Must run before any window or coordinate call: per-monitor DPI awareness makes every
/// coordinate a physical pixel, as the layout expects.
pub fn init_process() {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // Fails harmlessly if a manifest already set it.
    // SAFETY: plain Win32 call with a predefined constant.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn platform() -> Result<Platform, PlatformError> {
    init_process();
    let screens = Arc::new(WinScreens::default());
    Ok(Platform {
        capture: Arc::new(WinCapture::new()),
        injector: Arc::new(WinInjector::new(screens.clone())),
        screens,
    })
}
