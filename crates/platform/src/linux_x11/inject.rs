use std::sync::{Arc, Mutex};

use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::{X11Screens, connect};
use crate::keymap::hid_to_evdev;
use crate::{InputInjector, PlatformError, ScreenInfo};

pub struct X11Injector {
    conn: Arc<RustConnection>,
    root: u32,
    screens: Arc<X11Screens>,
    /// Hi-res wheel remainder per axis: X11 scrolls in whole notches (120 units).
    wheel: Mutex<(i32, i32)>,
}

fn os(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Os(e.to_string())
}

impl X11Injector {
    pub fn new(screens: Arc<X11Screens>) -> Result<Self, PlatformError> {
        let (conn, root) = connect()?;
        Ok(Self {
            conn,
            root,
            screens,
            wheel: Mutex::new((0, 0)),
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), PlatformError> {
        self.conn
            .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)
            .map_err(os)?;
        Ok(())
    }

    fn flush(&self) -> Result<(), PlatformError> {
        self.conn.flush().map_err(os)
    }

    fn click(&self, button: u8) -> Result<(), PlatformError> {
        self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
        self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)
    }
}

impl InputInjector for X11Injector {
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError> {
        let screens = self.screens.screens();
        let s = screens
            .iter()
            .find(|s| s.id == screen)
            .ok_or_else(|| os(format!("no screen {screen}")))?;
        let vx = s.origin_x + x.clamp(0, s.px_w as i32 - 1);
        let vy = s.origin_y + y.clamp(0, s.px_h as i32 - 1);
        // detail 0: absolute coordinates on `root`.
        self.fake(MOTION_NOTIFY_EVENT, 0, vx as i16, vy as i16)?;
        self.flush()
    }

    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        self.fake(MOTION_NOTIFY_EVENT, 1, dx as i16, dy as i16)?;
        self.flush()
    }

    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let evdev = hid_to_evdev(usage).ok_or_else(|| {
            PlatformError::Unsupported(format!("key {:#04x} has no evdev code", usage.0))
        })?;
        let keycode = u8::try_from(evdev + 8).map_err(os)?;
        self.fake(
            if down {
                KEY_PRESS_EVENT
            } else {
                KEY_RELEASE_EVENT
            },
            keycode,
            0,
            0,
        )?;
        self.flush()
    }

    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let detail = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 8,
            MouseButton::Forward => 9,
        };
        self.fake(
            if down {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            },
            detail,
            0,
            0,
        )?;
        self.flush()
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        let (mut rx, mut ry) = *self.wheel.lock().unwrap_or_else(|p| p.into_inner());
        rx += dx;
        ry += dy;
        while ry >= 120 {
            self.click(4)?;
            ry -= 120;
        }
        while ry <= -120 {
            self.click(5)?;
            ry += 120;
        }
        while rx >= 120 {
            self.click(7)?;
            rx -= 120;
        }
        while rx <= -120 {
            self.click(6)?;
            rx += 120;
        }
        *self.wheel.lock().unwrap_or_else(|p| p.into_inner()) = (rx, ry);
        self.flush()
    }

    fn text(&self, _text: &str) -> Result<(), PlatformError> {
        // TODO(M10): typing arbitrary characters needs temporary keysym remapping.
        Err(PlatformError::Unsupported("text input on X11".into()))
    }

    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError> {
        // TODO(M4b+): switch the XKB group that matches `tag.lang`.
        Err(PlatformError::Unsupported(format!(
            "layout switching on X11 ({})",
            tag.lang
        )))
    }

    fn set_locks(&self, locks: LockState) -> Result<(), PlatformError> {
        let current = self.screens.locks();
        for (want, have, key) in [
            (locks.caps, current.caps, HidUsage::CAPS_LOCK),
            (locks.num, current.num, HidUsage::NUM_LOCK),
        ] {
            if want != have {
                self.key(key, true)?;
                self.key(key, false)?;
            }
        }
        Ok(())
    }
}
