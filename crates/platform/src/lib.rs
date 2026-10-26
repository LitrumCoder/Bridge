//! OS integration (ARCHITECTURE §7): input capture and injection, screens; clipboard,
//! drag and drop, MIDI ports, network changes and overlays arrive with their milestones.
//! Each trait has per-OS implementations behind `cfg` and a scriptable mock.

use std::sync::Arc;

use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};

pub use bridge_core::LocalScreen;
pub use bridge_core::input::{CaptureEvent, Decision};

pub mod keymap;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
#[cfg(windows)]
pub mod windows;

/// The input platform of this OS, or why there is none.
pub fn native() -> Result<Platform, PlatformError> {
    #[cfg(windows)]
    {
        windows::platform()
    }
    #[cfg(target_os = "linux")]
    {
        // TODO(M4c): prefer the Wayland portals when WAYLAND_DISPLAY is set.
        linux_x11::platform()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // TODO(M4c): macOS.
        Err(PlatformError::Unsupported(
            "input sharing on this OS arrives in milestone 4c".into(),
        ))
    }
}

// TODO(M4c): macos (CGEventTap + CGEventPost), linux_wayland (portals + libei).
// TODO(M5): Clipboard. TODO(M6b): DragSource, DropTarget. TODO(M7): MidiPorts.
// TODO(M8): NetworkMonitor. TODO(M9): Overlay (edge glow, locator) in the UI process.

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlatformError {
    /// The user must grant something first (macOS Accessibility, a Wayland portal).
    #[error("permission required: {0}")]
    PermissionRequired(String),
    #[error("not supported here: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Os(String),
}

/// Receives captured events on the capture thread and decides synchronously.
pub trait CaptureSink: Send + Sync + 'static {
    fn on_event(&self, ev: CaptureEvent) -> Decision;
}

pub trait InputCapture: Send + Sync + 'static {
    /// Starts delivering events to `sink` from a dedicated thread.
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError>;
    /// Grab mode: swallowed events report relative motion, the local cursor is parked.
    fn set_grab(&self, grab: bool);
    /// Moves the local cursor (virtual desktop pixels).
    fn warp(&self, x: i32, y: i32);
    fn stop(&self);
}

pub trait InputInjector: Send + Sync + 'static {
    /// Absolute move to a pixel of local screen `screen`.
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError>;
    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError>;
    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError>;
    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError>;
    /// Wheel in 1/120 notches.
    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError>;
    fn text(&self, text: &str) -> Result<(), PlatformError>;
    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError>;
    fn set_locks(&self, locks: LockState) -> Result<(), PlatformError>;
}

pub trait ScreenInfo: Send + Sync + 'static {
    fn screens(&self) -> Vec<LocalScreen>;
    /// Active keyboard layout, for language sync.
    fn layout_tag(&self) -> Option<LayoutTag>;
    fn locks(&self) -> LockState;
}

/// The services of one OS, as the daemon wires them.
#[derive(Clone)]
pub struct Platform {
    pub capture: Arc<dyn InputCapture>,
    pub injector: Arc<dyn InputInjector>,
    pub screens: Arc<dyn ScreenInfo>,
}
