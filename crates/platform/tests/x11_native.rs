//! Real XInput2 capture and XTest injection against an X server (Xvfb in CI).
//! Opt-in: `BRIDGE_X11_TESTS=1` with `DISPLAY` set.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bridge_platform::linux_x11::{X11Capture, X11Injector, X11Screens};
use bridge_platform::{
    CaptureEvent, CaptureSink, Decision, InputCapture, InputInjector, ScreenInfo,
};
use bridge_protocol::input::HidUsage;

fn enabled() -> bool {
    std::env::var_os("BRIDGE_X11_TESTS").is_some() && std::env::var_os("DISPLAY").is_some()
}

#[derive(Default)]
struct Recorder(Mutex<Vec<CaptureEvent>>);

impl CaptureSink for Recorder {
    fn on_event(&self, ev: CaptureEvent) -> Decision {
        self.0.lock().unwrap().push(ev);
        Decision::Pass
    }
}

fn wait(rec: &Recorder, pred: impl Fn(&[CaptureEvent]) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if pred(&rec.0.lock().unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn xtest_injection_is_seen_by_xinput2_capture() {
    if !enabled() {
        eprintln!("skipped: set BRIDGE_X11_TESTS=1 and DISPLAY");
        return;
    }
    let screens = Arc::new(X11Screens::new().unwrap());
    let list = screens.screens();
    assert!(!list.is_empty(), "RandR reports at least one monitor");

    let rec = Arc::new(Recorder::default());
    // The X server only has XTest devices here, so do not filter them.
    let capture = X11Capture::new(false).unwrap();
    capture.start(rec.clone()).unwrap();
    let injector = X11Injector::new(screens.clone()).unwrap();
    // XWayland (e.g. WSLg) starts lazily; give the server a moment to wire up input.
    std::thread::sleep(Duration::from_millis(300));

    injector.key(HidUsage::A, true).unwrap();
    injector.key(HidUsage::A, false).unwrap();
    assert!(
        wait(&rec, |l| l.contains(&CaptureEvent::Key {
            usage: HidUsage::A,
            down: true,
            repeat: false
        })),
        "key down; captured: {:?}",
        rec.0.lock().unwrap()
    );
    assert!(
        wait(&rec, |l| l.contains(&CaptureEvent::Key {
            usage: HidUsage::A,
            down: false,
            repeat: false
        })),
        "key up"
    );

    // Absolute move lands where asked; relative moves show up as raw motion.
    let s = &list[0];
    injector.move_abs(s.id, 100, 80).unwrap();
    injector.move_rel(15, 0).unwrap();
    assert!(
        wait(&rec, |l| l.iter().any(
            |e| matches!(e, CaptureEvent::Motion { dx, .. } if *dx != 0)
        )),
        "raw motion with a delta"
    );

    // While grabbed, motion keeps arriving as raw deltas.
    rec.0.lock().unwrap().clear();
    capture.set_grab(true);
    injector.move_rel(-7, 3).unwrap();
    assert!(
        wait(&rec, |l| l
            .iter()
            .any(|e| matches!(e, CaptureEvent::Motion { .. }))),
        "motion while grabbed"
    );
    capture.set_grab(false);

    let locks = screens.locks();
    injector.key(HidUsage::CAPS_LOCK, true).unwrap();
    injector.key(HidUsage::CAPS_LOCK, false).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_ne!(
        screens.locks().caps,
        locks.caps,
        "Caps Lock toggled through XTest"
    );
    injector.set_locks(locks).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        screens.locks().caps,
        locks.caps,
        "set_locks restores the state"
    );
    capture.stop();
}
