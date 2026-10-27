//! Linux X11: XInput2 raw events to observe, active grabs to take over, XTest to inject,
//! RandR for monitors (ADR-0005). Pure Rust over the X protocol (x11rb), no libX11.

mod capture;
mod clipboard;
mod inject;

use std::sync::Arc;

use bridge_protocol::input::{LayoutTag, LockState};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};
use x11rb::rust_connection::RustConnection;

pub use capture::X11Capture;
pub use clipboard::X11Clipboard;
pub use inject::X11Injector;

use crate::{LocalScreen, Platform, PlatformError, ScreenInfo};

pub(crate) fn connect() -> Result<(Arc<RustConnection>, u32), PlatformError> {
    let (conn, screen) = x11rb::connect(None)
        .map_err(|e| PlatformError::Unsupported(format!("no X11 display: {e}")))?;
    let root = conn.setup().roots[screen].root;
    Ok((Arc::new(conn), root))
}

pub struct X11Screens {
    conn: Arc<RustConnection>,
    root: u32,
}

impl X11Screens {
    pub fn new() -> Result<Self, PlatformError> {
        let (conn, root) = connect()?;
        Ok(Self { conn, root })
    }

    fn pointer_mask(&self) -> Option<KeyButMask> {
        self.conn
            .query_pointer(self.root)
            .ok()?
            .reply()
            .ok()
            .map(|r| r.mask)
    }
}

impl ScreenInfo for X11Screens {
    fn screens(&self) -> Vec<LocalScreen> {
        let Some(reply) = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return Vec::new();
        };
        let mut monitors = reply.monitors;
        monitors.sort_by_key(|m| (m.x, m.y));
        monitors
            .iter()
            .enumerate()
            .map(|(i, m)| LocalScreen {
                id: i as u32,
                px_w: u32::from(m.width.max(1)),
                px_h: u32::from(m.height.max(1)),
                origin_x: i32::from(m.x),
                origin_y: i32::from(m.y),
                // X11 has no per-monitor scale; toolkits scale on their own.
                scale_permille: 1000,
                size_mm: (m.width_in_millimeters > 0 && m.height_in_millimeters > 0).then(|| {
                    (
                        f64::from(m.width_in_millimeters),
                        f64::from(m.height_in_millimeters),
                    )
                }),
            })
            .collect()
    }

    fn layout_tag(&self) -> Option<LayoutTag> {
        // TODO(M4b+): XKB group names for language sync on X11.
        None
    }

    fn locks(&self) -> LockState {
        let mask = self.pointer_mask().unwrap_or_default();
        // Lock = Caps Lock; Mod2 is Num Lock on every common keymap.
        LockState {
            caps: mask.contains(KeyButMask::LOCK),
            num: mask.contains(KeyButMask::MOD2),
            scroll: false,
        }
    }
}

pub fn platform() -> Result<Platform, PlatformError> {
    let screens = Arc::new(X11Screens::new()?);
    Ok(Platform {
        capture: Arc::new(X11Capture::new(true)?),
        injector: Arc::new(X11Injector::new(screens.clone())?),
        screens,
    })
}
