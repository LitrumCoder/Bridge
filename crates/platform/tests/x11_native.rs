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

    // A freshly started XWayland can drop the very first events; retry a few times.
    let down = CaptureEvent::Key {
        usage: HidUsage::A,
        down: true,
        repeat: false,
    };
    let mut seen = false;
    for _ in 0..5 {
        injector.key(HidUsage::A, true).unwrap();
        injector.key(HidUsage::A, false).unwrap();
        if wait(&rec, |l| l.contains(&down)) {
            seen = true;
            break;
        }
    }
    assert!(seen, "key down; captured: {:?}", rec.0.lock().unwrap());
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

#[derive(Default)]
struct Changes(Mutex<Vec<bridge_platform::ClipChange>>);

impl bridge_platform::ClipboardSink for Changes {
    fn changed(&self, change: bridge_platform::ClipChange) {
        self.0.lock().unwrap().push(change);
    }
}

struct Fixed(Vec<(bridge_platform::ClipFormat, Vec<u8>)>);

impl bridge_platform::ClipboardProvider for Fixed {
    fn render(&self, format: bridge_platform::ClipFormat) -> Option<Vec<u8>> {
        self.0
            .iter()
            .find(|(f, _)| *f == format)
            .map(|(_, d)| d.clone())
    }
}

/// Two clipboard clients on one display: one owns, the other sees the change and reads,
/// including a 3 MB image that must go through INCR.
#[test]
fn clipboard_ownership_targets_and_incr() {
    use bridge_platform::linux_x11::X11Clipboard;
    use bridge_platform::{ClipFormat, Clipboard};
    if !enabled() {
        eprintln!("skipped: set BRIDGE_X11_TESTS=1 and DISPLAY");
        return;
    }
    let (owner, watcher) = (X11Clipboard::new().unwrap(), X11Clipboard::new().unwrap());
    let (oc, wc) = (Arc::new(Changes::default()), Arc::new(Changes::default()));
    owner.start(oc.clone()).unwrap();
    watcher.start(wc.clone()).unwrap();
    let image: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    owner
        .announce(
            &[ClipFormat::TEXT, ClipFormat::PNG],
            Arc::new(Fixed(vec![
                (ClipFormat::TEXT, "привет".as_bytes().to_vec()),
                (ClipFormat::PNG, image.clone()),
            ])),
        )
        .unwrap();
    let start = Instant::now();
    let seen = loop {
        if let Some(c) = wc.0.lock().unwrap().iter().find(|c| !c.own).cloned() {
            break c;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "watcher sees the new owner"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(seen.formats, vec![ClipFormat::TEXT, ClipFormat::PNG]);
    assert!(
        oc.0.lock().unwrap().iter().any(|c| c.own),
        "the owner sees its own takeover"
    );
    assert_eq!(
        watcher.read(ClipFormat::TEXT).unwrap().unwrap(),
        "привет".as_bytes()
    );
    assert_eq!(watcher.read(ClipFormat::PNG).unwrap().unwrap(), image);
    assert_eq!(watcher.read(ClipFormat::HTML).unwrap(), None);
    owner.stop();
    watcher.stop();
}

struct DragRec(Mutex<Vec<Vec<std::path::PathBuf>>>);

impl bridge_platform::DragSink for DragRec {
    fn drag_entered(&self, paths: Vec<std::path::PathBuf>) {
        self.0.lock().unwrap().push(paths);
    }
    fn drag_left(&self) {}
}

struct Ready;

impl bridge_platform::DropProgress for Ready {
    fn wait_complete(&self, _timeout: Duration) -> bool {
        true
    }
}

struct DropRec(Mutex<Option<bool>>);

impl bridge_platform::DropSink for DropRec {
    fn dropped(&self, accepted: bool) {
        *self.0.lock().unwrap() = Some(accepted);
    }
}

/// Both XDND roles on one display: our drag session (the receiving side of a
/// cross-device drag) talks to our edge strips (the sending side), which read the
/// file list and then refuse the local drop, as they must.
#[test]
fn xdnd_drag_session_reaches_the_edge_strips() {
    use bridge_platform::linux_x11::{XdndSource, XdndTarget};
    use bridge_platform::{DragSource, DropItem, DropTarget};
    use bridge_protocol::input::MouseButton;
    if !enabled() {
        eprintln!("skipped: set BRIDGE_X11_TESTS=1 and DISPLAY");
        return;
    }
    let screens = Arc::new(X11Screens::new().unwrap());
    let s = screens.screens()[0];
    let injector = X11Injector::new(screens.clone()).unwrap();
    let rec = Arc::new(DragRec(Mutex::new(Vec::new())));
    let edges = XdndSource::new().unwrap();
    edges.start(rec.clone()).unwrap();
    injector.move_abs(s.id, s.px_w as i32 - 1, 300).unwrap();
    injector.button(MouseButton::Left, true).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let staged = std::path::PathBuf::from("/tmp/bridge xdnd/файл.txt");
    let dropped = Arc::new(DropRec(Mutex::new(None)));
    XdndTarget::new()
        .unwrap()
        .begin(
            vec![DropItem {
                path: staged.clone(),
                is_dir: false,
            }],
            Arc::new(Ready),
            dropped.clone(),
        )
        .unwrap();
    let start = Instant::now();
    while rec.0.lock().unwrap().is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    injector.button(MouseButton::Left, false).unwrap();
    assert_eq!(
        rec.0.lock().unwrap().first(),
        Some(&vec![staged]),
        "the strip read the uri-list"
    );
    let start = Instant::now();
    while dropped.0.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        *dropped.0.lock().unwrap(),
        Some(false),
        "the edge refuses the local drop"
    );
    edges.stop();
}
