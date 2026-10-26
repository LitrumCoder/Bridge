use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use bridge_protocol::input::MouseButton;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _, EventMask, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask as CoreMask, GrabMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use super::connect;
use crate::keymap::evdev_to_hid;
use crate::{CaptureEvent, CaptureSink, InputCapture, PlatformError};

/// XInput2 device id meaning "all master devices".
const ALL_MASTER: u16 = 1;

struct Shared {
    conn: Arc<RustConnection>,
    root: u32,
    wake_window: u32,
    grab: AtomicBool,
    pos: Mutex<(i32, i32)>,
    park: Mutex<(i32, i32)>,
    xtest_devices: HashSet<u16>,
    sink: Mutex<Option<Arc<dyn CaptureSink>>>,
}

pub struct X11Capture {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn fp(v: &Fp3232) -> f64 {
    f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0
}

/// Relative x/y of a raw motion: valuators 0 and 1, in mask order.
fn raw_delta(mask: &[u32], values: &[Fp3232]) -> (f64, f64) {
    let (mut dx, mut dy) = (0.0, 0.0);
    let mut idx = 0;
    for bit in 0..(mask.len() * 32) {
        if mask[bit / 32] & (1 << (bit % 32)) == 0 {
            continue;
        }
        let Some(v) = values.get(idx) else { break };
        match bit {
            0 => dx = fp(v),
            1 => dy = fp(v),
            _ => {}
        }
        idx += 1;
    }
    (dx, dy)
}

fn button(detail: u32) -> Option<MouseButton> {
    Some(match detail {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        8 => MouseButton::Back,
        9 => MouseButton::Forward,
        _ => return None,
    })
}

impl X11Capture {
    /// `ignore_xtest`: drop events from XTest devices, which is where our own injection
    /// (and tools like xdotool) come from. Tests on Xvfb turn it off.
    pub fn new(ignore_xtest: bool) -> Result<Self, PlatformError> {
        let (conn, root) = connect()?;
        let os = |e: &dyn std::fmt::Display| PlatformError::Os(e.to_string());
        let v = conn
            .xinput_xi_query_version(2, 2)
            .map_err(|e| os(&e))?
            .reply()
            .map_err(|e| os(&e))?;
        if v.major_version < 2 {
            return Err(PlatformError::Unsupported("XInput 2 is required".into()));
        }
        let mut xtest_devices = HashSet::new();
        if ignore_xtest {
            let devices = conn
                .xinput_xi_query_device(0u16)
                .map_err(|e| os(&e))?
                .reply()
                .map_err(|e| os(&e))?;
            for d in devices.infos {
                if String::from_utf8_lossy(&d.name).contains("XTEST") {
                    xtest_devices.insert(d.deviceid);
                }
            }
        }
        let wake_window = conn.generate_id().map_err(|e| os(&e))?;
        conn.create_window(
            0,
            wake_window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(|e| os(&e))?;
        let mask = XIEventMask::RAW_MOTION
            | XIEventMask::RAW_KEY_PRESS
            | XIEventMask::RAW_KEY_RELEASE
            | XIEventMask::RAW_BUTTON_PRESS
            | XIEventMask::RAW_BUTTON_RELEASE;
        conn.xinput_xi_select_events(
            root,
            &[EventMask {
                deviceid: ALL_MASTER,
                mask: vec![mask],
            }],
        )
        .map_err(|e| os(&e))?;
        conn.flush().map_err(|e| os(&e))?;
        let pointer = conn
            .query_pointer(root)
            .map_err(|e| os(&e))?
            .reply()
            .map_err(|e| os(&e))?;
        Ok(Self {
            shared: Arc::new(Shared {
                conn,
                root,
                wake_window,
                grab: AtomicBool::new(false),
                pos: Mutex::new((i32::from(pointer.root_x), i32::from(pointer.root_y))),
                park: Mutex::new((0, 0)),
                xtest_devices,
                sink: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        })
    }
}

fn run(shared: Arc<Shared>) {
    let mut pressed: HashSet<u32> = HashSet::new();
    loop {
        let Ok(event) = shared.conn.wait_for_event() else {
            return;
        };
        let sink = shared
            .sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(sink) = sink else { return };
        let skip = |source: u16| shared.xtest_devices.contains(&source);
        let ev = match event {
            Event::ClientMessage(m) if m.window == shared.wake_window => return,
            Event::XinputRawMotion(e) if !skip(e.sourceid) => {
                let (dx, dy) = raw_delta(&e.valuator_mask, &e.axisvalues);
                let grabbing = shared.grab.load(Ordering::Acquire);
                let (x, y) = if grabbing {
                    *shared.park.lock().unwrap_or_else(|p| p.into_inner())
                } else {
                    *shared.pos.lock().unwrap_or_else(|p| p.into_inner())
                };
                if !grabbing
                    && let Some(r) = shared
                        .conn
                        .query_pointer(shared.root)
                        .ok()
                        .and_then(|c| c.reply().ok())
                {
                    *shared.pos.lock().unwrap_or_else(|p| p.into_inner()) =
                        (i32::from(r.root_x), i32::from(r.root_y));
                }
                Some(CaptureEvent::Motion {
                    x,
                    y,
                    dx: dx.round() as i32,
                    dy: dy.round() as i32,
                })
            }
            Event::XinputRawKeyPress(e) if !skip(e.sourceid) => {
                let repeat = !pressed.insert(e.detail);
                key(e.detail, true, repeat)
            }
            Event::XinputRawKeyRelease(e) if !skip(e.sourceid) => {
                pressed.remove(&e.detail);
                key(e.detail, false, false)
            }
            Event::XinputRawButtonPress(e) | Event::XinputRawButtonRelease(e)
                if !skip(e.sourceid) =>
            {
                let down = e.event_type == xinput::RAW_BUTTON_PRESS_EVENT;
                match e.detail {
                    4 | 5 if down => Some(CaptureEvent::Wheel {
                        dx: 0,
                        dy: if e.detail == 4 { 120 } else { -120 },
                    }),
                    6 | 7 if down => Some(CaptureEvent::Wheel {
                        dx: if e.detail == 7 { 120 } else { -120 },
                        dy: 0,
                    }),
                    d => button(d).map(|button| CaptureEvent::Button { button, down }),
                }
            }
            _ => None,
        };
        // X11 cannot hide an event that already happened; while grabbed, our grab keeps
        // events from other clients, so the decision has nothing left to do here.
        if let Some(ev) = ev {
            sink.on_event(ev);
        }
    }
}

fn key(keycode: u32, down: bool, repeat: bool) -> Option<CaptureEvent> {
    let evdev = u16::try_from(keycode.checked_sub(8)?).ok()?;
    evdev_to_hid(evdev).map(|usage| CaptureEvent::Key {
        usage,
        down,
        repeat,
    })
}

impl InputCapture for X11Capture {
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError> {
        *self.shared.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        let mut guard = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            let shared = self.shared.clone();
            *guard = Some(
                std::thread::Builder::new()
                    .name("bridge-x11-input".into())
                    .spawn(move || run(shared))
                    .map_err(|e| PlatformError::Os(e.to_string()))?,
            );
        }
        Ok(())
    }

    fn set_grab(&self, grab: bool) {
        let s = &self.shared;
        let c = &s.conn;
        if grab {
            let screen = &c.setup().roots[0];
            let park = (
                i32::from(screen.width_in_pixels) / 2,
                i32::from(screen.height_in_pixels) / 2,
            );
            *s.park.lock().unwrap_or_else(|p| p.into_inner()) = park;
            let mask = CoreMask::POINTER_MOTION | CoreMask::BUTTON_PRESS | CoreMask::BUTTON_RELEASE;
            let _ = c.grab_pointer(
                false,
                s.root,
                mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                CURRENT_TIME,
            );
            let _ = c.grab_keyboard(
                false,
                s.root,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
            let _ = c.warp_pointer(NONE, s.root, 0, 0, 0, 0, park.0 as i16, park.1 as i16);
        } else {
            let _ = c.ungrab_pointer(CURRENT_TIME);
            let _ = c.ungrab_keyboard(CURRENT_TIME);
        }
        s.grab.store(grab, Ordering::Release);
        let _ = c.flush();
    }

    fn warp(&self, x: i32, y: i32) {
        let s = &self.shared;
        let _ = s
            .conn
            .warp_pointer(NONE, s.root, 0, 0, 0, 0, x as i16, y as i16);
        let _ = s.conn.flush();
        *s.pos.lock().unwrap_or_else(|p| p.into_inner()) = (x, y);
    }

    fn stop(&self) {
        let s = &self.shared;
        *s.sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if s.grab.swap(false, Ordering::AcqRel) {
            let _ = s.conn.ungrab_pointer(CURRENT_TIME);
            let _ = s.conn.ungrab_keyboard(CURRENT_TIME);
        }
        let wake = ClientMessageEvent::new(32, s.wake_window, 0u32, [0u32; 5]);
        let _ = s
            .conn
            .send_event(false, s.wake_window, CoreMask::NO_EVENT, wake);
        let _ = s.conn.flush();
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_valuators_follow_the_mask() {
        let v = |i: i32| Fp3232 {
            integral: i,
            frac: 0,
        };
        assert_eq!(raw_delta(&[0b11], &[v(3), v(-2)]), (3.0, -2.0));
        assert_eq!(raw_delta(&[0b10], &[v(5)]), (0.0, 5.0));
        assert_eq!(
            raw_delta(&[0b101], &[v(1), v(9)]),
            (1.0, 0.0),
            "valuator 2 is not y"
        );
        assert_eq!(raw_delta(&[0b11], &[v(1)]), (1.0, 0.0), "short value list");
        let half = Fp3232 {
            integral: 0,
            frac: 1 << 31,
        };
        assert_eq!(raw_delta(&[0b1], &[half]).0, 0.5);
        assert_eq!(
            key(38, true, false),
            Some(CaptureEvent::Key {
                usage: bridge_protocol::input::HidUsage::A,
                down: true,
                repeat: false
            })
        );
        assert_eq!(key(3, true, false), None);
        assert_eq!(button(8), Some(MouseButton::Back));
        assert_eq!(button(4), None);
    }
}
