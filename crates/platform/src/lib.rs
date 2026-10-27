//! OS integration (ARCHITECTURE §7): input capture and injection, screens; clipboard,
//! drag and drop, MIDI ports, network changes and overlays arrive with their milestones.
//! Each trait has per-OS implementations behind `cfg` and a scriptable mock.

use std::sync::Arc;

pub use bridge_protocol::clipboard::ClipFormat;
use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};

pub mod clipfmt;

pub use bridge_core::LocalScreen;
pub use bridge_core::input::{CaptureEvent, Decision};

pub mod keymap;
#[cfg(target_os = "linux")]
pub mod linux_wayland;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(target_os = "macos")]
pub mod macos;
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
        if linux_wayland::is_wayland_session() {
            linux_wayland::platform()
        } else {
            linux_x11::platform()
        }
    }
    #[cfg(target_os = "macos")]
    {
        macos::platform()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err(PlatformError::Unsupported(
            "input sharing is not available on this OS".into(),
        ))
    }
}

/// The clipboard of this OS, or why it cannot be shared.
pub fn native_clipboard() -> Result<Arc<dyn Clipboard>, PlatformError> {
    #[cfg(windows)]
    {
        Ok(Arc::new(windows::WinClipboard::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() && std::env::var_os("DISPLAY").is_none() {
            // TODO(M5+): ext-data-control-v1 / zwlr-data-control-v1.
            return Err(PlatformError::Unsupported(
                "clipboard sharing on Wayland without XWayland is not implemented yet".into(),
            ));
        }
        Ok(Arc::new(linux_x11::X11Clipboard::new()?))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // TODO(macOS-final): NSPasteboard polling + data provider (owner deferred all
        // macOS work to the end of stage 1, ROADMAP).
        Err(PlatformError::Unsupported(
            "clipboard sharing is not available on this OS yet".into(),
        ))
    }
}

// TODO(M6b): DragSource, DropTarget. TODO(M7): MidiPorts.
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

/// What the local clipboard holds now. `own` marks changes we made ourselves (after
/// `announce`), which must not be offered back; `concealed` marks contents a password
/// manager asked to keep out of clipboard monitors.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ClipChange {
    pub formats: Vec<ClipFormat>,
    pub own: bool,
    pub concealed: bool,
}

pub trait ClipboardSink: Send + Sync + 'static {
    /// Called on the clipboard thread; must not block.
    fn changed(&self, change: ClipChange);
}

/// Renders an announced format when an application pastes. May block (it waits for
/// the network); return `None` when the data cannot be had.
pub trait ClipboardProvider: Send + Sync + 'static {
    fn render(&self, format: ClipFormat) -> Option<Vec<u8>>;
}

/// The OS clipboard in wire formats (`ClipFormat`): text is UTF-8 with `\n`, HTML is
/// UTF-8 markup, images are PNG.
pub trait Clipboard: Send + Sync + 'static {
    fn start(&self, sink: Arc<dyn ClipboardSink>) -> Result<(), PlatformError>;
    /// Reads one format of the current contents (`None`: not present).
    fn read(&self, format: ClipFormat) -> Result<Option<Vec<u8>>, PlatformError>;
    /// Takes the clipboard and promises `formats`; `provider` renders them on paste.
    fn announce(
        &self,
        formats: &[ClipFormat],
        provider: Arc<dyn ClipboardProvider>,
    ) -> Result<(), PlatformError>;
    fn stop(&self);
}

/// The services of one OS, as the daemon wires them.
#[derive(Clone)]
pub struct Platform {
    pub capture: Arc<dyn InputCapture>,
    pub injector: Arc<dyn InputInjector>,
    pub screens: Arc<dyn ScreenInfo>,
}
