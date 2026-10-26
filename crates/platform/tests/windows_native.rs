//! Real hooks and SendInput on this Windows session. Opt-in (`BRIDGE_PLATFORM_TESTS=1`),
//! because it touches the live desktop: it presses F24 (absent on real keyboards) and
//! moves the cursor briefly, putting it back afterwards.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_platform::windows::{WinCapture, WinInjector, WinScreens};
use bridge_platform::{
    CaptureEvent, CaptureSink, Decision, InputCapture, InputInjector, ScreenInfo,
};
use bridge_protocol::input::HidUsage;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

const F24: HidUsage = HidUsage(0x73);

fn enabled() -> bool {
    std::env::var_os("BRIDGE_PLATFORM_TESTS").is_some()
}

#[derive(Default)]
struct Recorder(Mutex<Vec<CaptureEvent>>);

impl CaptureSink for Recorder {
    fn on_event(&self, ev: CaptureEvent) -> Decision {
        self.0.lock().unwrap().push(ev);
        // Swallow our test key so it never reaches an application.
        match ev {
            CaptureEvent::Key { usage, .. } if usage == F24 => Decision::Swallow,
            _ => Decision::Pass,
        }
    }
}

/// Injects F24 as a foreign program would (no Bridge marker), so the hook sees it.
fn foreign_f24(down: bool) {
    let flags = KEYEVENTF_SCANCODE | if down { 0 } else { KEYEVENTF_KEYUP };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: 0x76,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
}

fn cursor() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

#[test]
fn hook_sees_foreign_keys_but_not_our_own_injection() {
    if !enabled() {
        eprintln!("skipped: set BRIDGE_PLATFORM_TESTS=1");
        return;
    }
    let rec = Arc::new(Recorder::default());
    let capture = WinCapture::new();
    capture.start(rec.clone()).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    foreign_f24(true);
    foreign_f24(false);
    std::thread::sleep(Duration::from_millis(200));
    let seen = rec.0.lock().unwrap().clone();
    assert!(
        seen.contains(&CaptureEvent::Key {
            usage: F24,
            down: true,
            repeat: false
        }),
        "{seen:?}"
    );
    assert!(seen.contains(&CaptureEvent::Key {
        usage: F24,
        down: false,
        repeat: false
    }));

    rec.0.lock().unwrap().clear();
    let injector = WinInjector::new(Arc::new(WinScreens::default()));
    injector.key(F24, true).unwrap();
    injector.key(F24, false).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let own = rec.0.lock().unwrap().clone();
    assert!(
        !own.iter()
            .any(|e| matches!(e, CaptureEvent::Key { usage, .. } if *usage == F24)),
        "our own injection must not loop back: {own:?}"
    );
    capture.stop();
}

#[test]
fn screens_and_absolute_moves_agree() {
    if !enabled() {
        eprintln!("skipped: set BRIDGE_PLATFORM_TESTS=1");
        return;
    }
    let screens = Arc::new(WinScreens::default());
    let list = screens.screens();
    assert!(!list.is_empty());
    for s in &list {
        assert!(s.px_w > 0 && s.px_h > 0 && s.scale_permille >= 1000);
    }
    let before = cursor();
    let injector = WinInjector::new(screens.clone());
    let s = &list[0];
    injector.move_abs(s.id, 100, 120).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let (x, y) = cursor();
    let capture = WinCapture::new();
    capture.warp(before.0, before.1);
    assert!(
        (x - (s.origin_x + 100)).abs() <= 1 && (y - (s.origin_y + 120)).abs() <= 1,
        "cursor at {x},{y}"
    );
    assert!(screens.layout_tag().is_some_and(|t| !t.lang.is_empty()));
}
