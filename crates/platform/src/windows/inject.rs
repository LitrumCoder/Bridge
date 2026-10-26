use std::sync::Arc;

use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, KLF_ACTIVATE, LoadKeyboardLayoutW,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, PostMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WM_INPUTLANGCHANGEREQUEST, XBUTTON1, XBUTTON2,
};

use super::INJECTED_MARK;
use super::screens::WinScreens;
use crate::keymap::hid_to_set1;
use crate::{InputInjector, PlatformError, ScreenInfo};

pub struct WinInjector {
    screens: Arc<WinScreens>,
}

impl WinInjector {
    pub fn new(screens: Arc<WinScreens>) -> Self {
        Self { screens }
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARK,
            },
        },
    }
}

fn keyboard(scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARK,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), PlatformError> {
    // SAFETY: `inputs` is a valid slice of initialised INPUT structures.
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        // UIPI blocks input into elevated windows silently; this is the usual cause.
        Err(PlatformError::Os(
            "SendInput was blocked (elevated window or secure desktop?)".into(),
        ))
    }
}

fn key_input(usage: HidUsage, down: bool) -> Option<INPUT> {
    let scan = hid_to_set1(usage)?;
    let mut flags = KEYEVENTF_SCANCODE;
    if scan & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    Some(keyboard(scan & 0xFF, flags))
}

impl InputInjector for WinInjector {
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError> {
        let screens = self.screens.cached();
        let s = screens
            .iter()
            .find(|s| s.id == screen)
            .ok_or_else(|| PlatformError::Os(format!("no screen {screen}")))?;
        let (vx, vy) = (
            s.origin_x + x.clamp(0, s.px_w as i32 - 1),
            s.origin_y + y.clamp(0, s.px_h as i32 - 1),
        );
        // SAFETY: GetSystemMetrics has no preconditions.
        let (left, top, w, h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        // Absolute input is normalised to 0..=65535 across the virtual desktop.
        let nx = ((i64::from(vx - left) * 65535 + i64::from(w - 1) / 2) / i64::from(w - 1)) as i32;
        let ny = ((i64::from(vy - top) * 65535 + i64::from(h - 1) / 2) / i64::from(h - 1)) as i32;
        send(&[mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let input = key_input(usage, down).ok_or_else(|| {
            PlatformError::Unsupported(format!("key {:#04x} has no Windows scan code", usage.0))
        })?;
        send(&[input])
    }

    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
        };
        send(&[mouse(0, 0, data, flags)])
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            inputs.push(mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            Ok(())
        } else {
            send(&inputs)
        }
    }

    fn text(&self, text: &str) -> Result<(), PlatformError> {
        let inputs: Vec<INPUT> = text
            .encode_utf16()
            .flat_map(|unit| {
                [
                    keyboard(unit, KEYEVENTF_UNICODE),
                    keyboard(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ]
            })
            .collect();
        if inputs.is_empty() {
            Ok(())
        } else {
            send(&inputs)
        }
    }

    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError> {
        let klid: Vec<u16> = tag.id.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `klid` is a NUL-terminated UTF-16 string that lives across the calls.
        unsafe {
            let hkl = LoadKeyboardLayoutW(klid.as_ptr(), KLF_ACTIVATE);
            if hkl.is_null() {
                return Err(PlatformError::Unsupported(format!(
                    "layout {} is not installed",
                    tag.id
                )));
            }
            PostMessageW(
                GetForegroundWindow(),
                WM_INPUTLANGCHANGEREQUEST,
                0,
                hkl as isize,
            );
        }
        Ok(())
    }

    fn set_locks(&self, locks: LockState) -> Result<(), PlatformError> {
        let current = self.screens.locks();
        for (want, have, key) in [
            (locks.caps, current.caps, HidUsage::CAPS_LOCK),
            (locks.num, current.num, HidUsage::NUM_LOCK),
            (locks.scroll, current.scroll, HidUsage(0x47)),
        ] {
            if want != have {
                self.key(key, true)?;
                self.key(key, false)?;
            }
        }
        Ok(())
    }
}
