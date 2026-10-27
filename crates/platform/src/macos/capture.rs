//! A session event tap on its own thread with a `CFRunLoop`. The callback asks the sink
//! for a decision and returns NULL to swallow. macOS disables a tap that is too slow
//! (`TapDisabledByTimeout`) — we enable it again; a tap disabled by the user
//! (Accessibility revoked) is torn down at once, as a live tap could hang input.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;

use bridge_protocol::input::MouseButton;
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CGPoint, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGEvent, CGEventField, CGEventMask, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
    CGWarpMouseCursorPosition,
};

use super::{INJECTED_MARK, cursor};
use crate::keymap::mac_to_hid;
use crate::{CaptureEvent, CaptureSink, Decision, InputCapture, PlatformError};

const KVK_CAPS_LOCK: u16 = 0x39;

struct TapState {
    sink: RwLock<Option<Arc<dyn CaptureSink>>>,
    grab: AtomicBool,
    last: Mutex<(i32, i32)>,
    park: Mutex<(i32, i32)>,
    held_modifiers: Mutex<HashSet<u16>>,
    tap: AtomicPtr<CFMachPort>,
}

static STATE: OnceLock<TapState> = OnceLock::new();

fn state() -> &'static TapState {
    STATE.get_or_init(|| TapState {
        sink: RwLock::new(None),
        grab: AtomicBool::new(false),
        last: Mutex::new((0, 0)),
        park: Mutex::new((0, 0)),
        held_modifiers: Mutex::new(HashSet::new()),
        tap: AtomicPtr::new(std::ptr::null_mut()),
    })
}

fn decide(ev: CaptureEvent) -> Decision {
    let sink = state()
        .sink
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    sink.map_or(Decision::Pass, |s| s.on_event(ev))
}

fn mask_of(types: &[CGEventType]) -> CGEventMask {
    types.iter().fold(0, |m, t| m | (1u64 << t.0))
}

fn point(p: CGPoint) -> (i32, i32) {
    (p.x.round() as i32, p.y.round() as i32)
}

fn other_button(number: i64) -> Option<MouseButton> {
    Some(match number {
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => return None,
    })
}

/// Lines (or points for trackpads, 10 per line) → 1/120 notches.
fn wheel_units(lines: i64, points: f64, continuous: bool) -> i32 {
    if continuous {
        (points * 12.0).round() as i32
    } else {
        (lines * 120) as i32
    }
}

/// Translates a tapped event; `None` for events we do not share.
fn translate(ty: CGEventType, ev: &CGEvent) -> Option<Vec<CaptureEvent>> {
    let e = Some(ev);
    let int = |f: CGEventField| CGEvent::integer_value_field(e, f);
    let button = |button, down| Some(vec![CaptureEvent::Button { button, down }]);
    match ty {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let s = state();
            let (x, y) = if s.grab.load(Ordering::Acquire) {
                *s.park.lock().unwrap_or_else(|p| p.into_inner())
            } else {
                *s.last.lock().unwrap_or_else(|p| p.into_inner())
            };
            Some(vec![CaptureEvent::Motion {
                x,
                y,
                dx: int(CGEventField::MouseEventDeltaX) as i32,
                dy: int(CGEventField::MouseEventDeltaY) as i32,
            }])
        }
        CGEventType::LeftMouseDown => button(MouseButton::Left, true),
        CGEventType::LeftMouseUp => button(MouseButton::Left, false),
        CGEventType::RightMouseDown => button(MouseButton::Right, true),
        CGEventType::RightMouseUp => button(MouseButton::Right, false),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let b = other_button(int(CGEventField::MouseEventButtonNumber))?;
            button(b, ty == CGEventType::OtherMouseDown)
        }
        CGEventType::ScrollWheel => {
            let continuous = int(CGEventField::ScrollWheelEventIsContinuous) != 0;
            let dbl = |f| CGEvent::double_value_field(e, f);
            let dy = wheel_units(
                int(CGEventField::ScrollWheelEventDeltaAxis1),
                dbl(CGEventField::ScrollWheelEventPointDeltaAxis1),
                continuous,
            );
            // macOS counts positive horizontal deltas to the left; the wire, to the right.
            let dx = -wheel_units(
                int(CGEventField::ScrollWheelEventDeltaAxis2),
                dbl(CGEventField::ScrollWheelEventPointDeltaAxis2),
                continuous,
            );
            (dx != 0 || dy != 0).then(|| vec![CaptureEvent::Wheel { dx, dy }])
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let usage = mac_to_hid(int(CGEventField::KeyboardEventKeycode) as u16)?;
            Some(vec![CaptureEvent::Key {
                usage,
                down: ty == CGEventType::KeyDown,
                repeat: int(CGEventField::KeyboardEventAutorepeat) != 0,
            }])
        }
        CGEventType::FlagsChanged => {
            let code = int(CGEventField::KeyboardEventKeycode) as u16;
            let usage = mac_to_hid(code)?;
            if code == KVK_CAPS_LOCK {
                // Caps Lock reports a state toggle, not press and release.
                return Some(vec![
                    CaptureEvent::Key {
                        usage,
                        down: true,
                        repeat: false,
                    },
                    CaptureEvent::Key {
                        usage,
                        down: false,
                        repeat: false,
                    },
                ]);
            }
            let mut held = state()
                .held_modifiers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let down = !held.contains(&code);
            if down {
                held.insert(code);
            } else {
                held.remove(&code);
            }
            Some(vec![CaptureEvent::Key {
                usage,
                down,
                repeat: false,
            }])
        }
        _ => None,
    }
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    _user: *mut c_void,
) -> *mut CGEvent {
    let s = state();
    let passed = event.as_ptr();
    if ty == CGEventType::TapDisabledByTimeout {
        let tap = s.tap.load(Ordering::Acquire);
        if let Some(tap) = NonNull::new(tap) {
            // SAFETY: the tap lives until the run loop thread exits, which is this thread.
            CGEvent::tap_enable(unsafe { tap.as_ref() }, true);
        }
        return passed;
    }
    if ty == CGEventType::TapDisabledByUserInput {
        *s.sink.write().unwrap_or_else(|p| p.into_inner()) = None;
        s.grab.store(false, Ordering::Release);
        let _ = CGAssociateMouseAndMouseCursorPosition(true);
        if let Some(rl) = CFRunLoop::current() {
            rl.stop();
        }
        return passed;
    }
    // SAFETY: the system hands us a valid event for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_MARK {
        return passed;
    }
    let Some(events) = translate(ty, ev) else {
        return passed;
    };
    let mut decision = Decision::Pass;
    for e in events {
        if decide(e) == Decision::Swallow {
            decision = Decision::Swallow;
        }
    }
    let moved = matches!(
        ty,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    );
    if moved && decision == Decision::Pass && !s.grab.load(Ordering::Acquire) {
        *s.last.lock().unwrap_or_else(|p| p.into_inner()) = point(CGEvent::location(Some(ev)));
    }
    match decision {
        Decision::Pass => passed,
        Decision::Swallow => std::ptr::null_mut(),
    }
}

/// The tap thread's run loop, stopped from `stop()`.
struct RunLoopHandle(CFRetained<CFRunLoop>);
// SAFETY: CFRunLoopStop and CFRunLoopWakeUp are documented as safe from any thread;
// nothing else is called through this handle.
unsafe impl Send for RunLoopHandle {}

#[derive(Default)]
pub struct MacCapture {
    thread: Mutex<Option<(JoinHandle<()>, RunLoopHandle)>>,
}

impl MacCapture {
    pub fn new() -> Self {
        Self::default()
    }
}

fn run_tap(ready: std::sync::mpsc::Sender<Result<RunLoopHandle, PlatformError>>) {
    let mask = mask_of(&[
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ]);
    // SAFETY: the callback is a 'static function that only touches thread-safe state.
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            std::ptr::null_mut(),
        )
    };
    let Some(tap) = tap else {
        let _ = ready.send(Err(PlatformError::PermissionRequired(
            "macOS refused the event tap: allow Bridge in Privacy & Security → Accessibility \
             and Input Monitoring"
                .into(),
        )));
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
        let _ = ready.send(Err(PlatformError::Os(
            "cannot create a run loop source".into(),
        )));
        return;
    };
    let Some(rl) = CFRunLoop::current() else {
        let _ = ready.send(Err(PlatformError::Os("no run loop".into())));
        return;
    };
    // SAFETY: reading a constant provided by CoreFoundation.
    let common = unsafe { kCFRunLoopCommonModes };
    rl.add_source(Some(&source), common);
    state()
        .tap
        .store(CFRetained::as_ptr(&tap).as_ptr(), Ordering::Release);
    CGEvent::tap_enable(&tap, true);
    let _ = ready.send(Ok(RunLoopHandle(rl.clone())));
    CFRunLoop::run();
    state().tap.store(std::ptr::null_mut(), Ordering::Release);
    CGEvent::tap_enable(&tap, false);
    rl.remove_source(Some(&source), common);
    tap.invalidate();
}

impl InputCapture for MacCapture {
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError> {
        let s = state();
        *s.sink.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        *s.last.lock().unwrap_or_else(|p| p.into_inner()) = point(cursor());
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.as_ref().is_some_and(|(h, _)| !h.is_finished()) {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("bridge-mac-tap".into())
            .spawn(move || run_tap(tx))
            .map_err(|e| PlatformError::Os(e.to_string()))?;
        let rl = rx
            .recv()
            .map_err(|_| PlatformError::Os("event tap thread died".into()))??;
        *guard = Some((handle, rl));
        Ok(())
    }

    fn set_grab(&self, grab: bool) {
        let s = state();
        if grab {
            *s.park.lock().unwrap_or_else(|p| p.into_inner()) = point(cursor());
        }
        // Detached, the cursor stays put and events still carry the deltas.
        let _ = CGAssociateMouseAndMouseCursorPosition(!grab);
        s.grab.store(grab, Ordering::Release);
    }

    fn warp(&self, x: i32, y: i32) {
        let _ = CGWarpMouseCursorPosition(CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        });
        // A warp freezes the mouse for a moment unless we re-associate right away.
        if !state().grab.load(Ordering::Acquire) {
            let _ = CGAssociateMouseAndMouseCursorPosition(true);
        }
        *state().last.lock().unwrap_or_else(|p| p.into_inner()) = (x, y);
    }

    fn stop(&self) {
        let s = state();
        *s.sink.write().unwrap_or_else(|p| p.into_inner()) = None;
        if s.grab.swap(false, Ordering::AcqRel) {
            let _ = CGAssociateMouseAndMouseCursorPosition(true);
        }
        if let Some((handle, rl)) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            rl.0.stop();
            let _ = handle.join();
        }
        s.held_modifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_and_buttons() {
        assert_eq!(wheel_units(1, 0.0, false), 120);
        assert_eq!(wheel_units(-2, 0.0, false), -240);
        assert_eq!(wheel_units(0, 10.0, true), 120);
        assert_eq!(other_button(2), Some(MouseButton::Middle));
        assert_eq!(other_button(7), None);
        assert_eq!(
            mask_of(&[CGEventType::KeyDown, CGEventType::KeyUp]),
            0b1100_0000_0000
        );
    }
}
