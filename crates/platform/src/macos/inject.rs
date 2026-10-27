use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bridge_protocol::input::{HidUsage, LayoutTag, LockState, MouseButton};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};

use super::{INJECTED_MARK, MacScreens, cursor};
use crate::keymap::hid_to_mac;
use crate::{InputInjector, PlatformError, ScreenInfo};

const DOUBLE_CLICK: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Buttons {
    held: Vec<MouseButton>,
    last_down: Option<(MouseButton, Instant, (i32, i32))>,
    clicks: i64,
}

pub struct MacInjector {
    screens: Arc<MacScreens>,
    buttons: Mutex<Buttons>,
}

impl MacInjector {
    pub fn new(screens: Arc<MacScreens>) -> Self {
        Self {
            screens,
            buttons: Mutex::new(Buttons::default()),
        }
    }

    fn post_mouse(&self, at: CGPoint) -> Result<(), PlatformError> {
        let held = self
            .buttons
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .held
            .first()
            .copied();
        // While a button is held macOS expects drag events, or apps never see the drag.
        let (ty, button) = match held {
            Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        post(CGEvent::new_mouse_event(None, ty, at, button))
    }

    fn clamp(&self, p: CGPoint) -> CGPoint {
        let screens = self.screens.screens();
        let inside = screens.iter().any(|s| {
            p.x >= f64::from(s.origin_x)
                && p.y >= f64::from(s.origin_y)
                && p.x < f64::from(s.origin_x) + f64::from(s.px_w)
                && p.y < f64::from(s.origin_y) + f64::from(s.px_h)
        });
        if inside || screens.is_empty() {
            return p;
        }
        let nearest = screens
            .iter()
            .map(|s| {
                let x = p.x.clamp(
                    f64::from(s.origin_x),
                    f64::from(s.origin_x + s.px_w as i32 - 1),
                );
                let y = p.y.clamp(
                    f64::from(s.origin_y),
                    f64::from(s.origin_y + s.px_h as i32 - 1),
                );
                CGPoint { x, y }
            })
            .min_by(|a, b| {
                let d = |q: &CGPoint| (q.x - p.x).powi(2) + (q.y - p.y).powi(2);
                d(a).total_cmp(&d(b))
            });
        nearest.unwrap_or(p)
    }
}

fn post(event: Option<CFRetained<CGEvent>>) -> Result<(), PlatformError> {
    let event = event.ok_or_else(|| PlatformError::Os("CGEventCreate failed".into()))?;
    CGEvent::set_integer_value_field(
        Some(&event),
        CGEventField::EventSourceUserData,
        INJECTED_MARK,
    );
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(())
}

/// Click count for `button` going down at `pos`: macOS apps read double clicks from it.
fn click_state(b: &mut Buttons, button: MouseButton, pos: (i32, i32), now: Instant) -> i64 {
    let near = |q: (i32, i32)| (q.0 - pos.0).abs() <= 4 && (q.1 - pos.1).abs() <= 4;
    b.clicks = match b.last_down {
        Some((prev, t, q)) if prev == button && now - t < DOUBLE_CLICK && near(q) => b.clicks + 1,
        _ => 1,
    };
    b.last_down = Some((button, now, pos));
    b.clicks
}

impl InputInjector for MacInjector {
    fn move_abs(&self, screen: u32, x: i32, y: i32) -> Result<(), PlatformError> {
        let screens = self.screens.screens();
        let s = screens
            .iter()
            .find(|s| s.id == screen)
            .ok_or_else(|| PlatformError::Os(format!("no screen {screen}")))?;
        let at = CGPoint {
            x: f64::from(s.origin_x + x.clamp(0, s.px_w as i32 - 1)),
            y: f64::from(s.origin_y + y.clamp(0, s.px_h as i32 - 1)),
        };
        self.post_mouse(at)
    }

    fn move_rel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        let c = cursor();
        let at = self.clamp(CGPoint {
            x: c.x + f64::from(dx),
            y: c.y + f64::from(dy),
        });
        self.post_mouse(at)
    }

    fn key(&self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let code = hid_to_mac(usage).ok_or_else(|| {
            PlatformError::Unsupported(format!("key {:#04x} has no macOS key code", usage.0))
        })?;
        post(CGEvent::new_keyboard_event(None, code, down))
    }

    fn button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let at = cursor();
        let (ty, cg, number) = match (button, down) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (b, d) => {
                let number = match b {
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    _ => 4,
                };
                let ty = if d {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                };
                (ty, CGMouseButton::Center, number)
            }
        };
        let clicks = {
            let mut b = self.buttons.lock().unwrap_or_else(|p| p.into_inner());
            if down {
                b.held.retain(|h| *h != button);
                b.held.push(button);
                click_state(&mut b, button, (at.x as i32, at.y as i32), Instant::now())
            } else {
                b.held.retain(|h| *h != button);
                b.clicks.max(1)
            }
        };
        let event = CGEvent::new_mouse_event(None, ty, at, cg);
        if let Some(e) = &event {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, clicks);
        }
        post(event)
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        // 1/120 notch → pixels at 10 px per notch; horizontal sign flips back to macOS's.
        post(CGEvent::new_scroll_wheel_event2(
            None,
            CGScrollEventUnit::Pixel,
            2,
            dy / 12,
            -dx / 12,
            0,
        ))
    }

    fn text(&self, text: &str) -> Result<(), PlatformError> {
        let units: Vec<u16> = text.encode_utf16().collect();
        // Key events carry at most 20 UTF-16 units each.
        for chunk in units.chunks(20) {
            for down in [true, false] {
                let event = CGEvent::new_keyboard_event(None, 0, down);
                if let Some(e) = &event {
                    // SAFETY: pointer and length describe `chunk`, which outlives the call.
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(e),
                            chunk.len() as _,
                            chunk.as_ptr(),
                        )
                    };
                }
                post(event)?;
            }
        }
        Ok(())
    }

    fn set_layout(&self, tag: &LayoutTag) -> Result<(), PlatformError> {
        // TODO(M4c+): TISSelectInputSource for language sync.
        Err(PlatformError::Unsupported(format!(
            "switching to layout {} is not implemented on macOS yet",
            tag.id
        )))
    }

    fn set_locks(&self, locks: LockState) -> Result<(), PlatformError> {
        // Only Caps Lock exists; a synthetic Caps Lock press toggles it on most macOS
        // versions but not all (LIMITATIONS).
        if locks.caps != self.screens.locks().caps {
            self.key(HidUsage::CAPS_LOCK, true)?;
            self.key(HidUsage::CAPS_LOCK, false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_clicks_count_up_only_nearby_and_soon() {
        let mut b = Buttons::default();
        let t = Instant::now();
        assert_eq!(click_state(&mut b, MouseButton::Left, (10, 10), t), 1);
        assert_eq!(
            click_state(
                &mut b,
                MouseButton::Left,
                (12, 11),
                t + Duration::from_millis(200)
            ),
            2
        );
        assert_eq!(
            click_state(
                &mut b,
                MouseButton::Left,
                (12, 11),
                t + Duration::from_millis(300)
            ),
            3
        );
        assert_eq!(
            click_state(
                &mut b,
                MouseButton::Right,
                (12, 11),
                t + Duration::from_millis(350)
            ),
            1
        );
        assert_eq!(
            click_state(
                &mut b,
                MouseButton::Right,
                (40, 11),
                t + Duration::from_millis(400)
            ),
            1
        );
        assert_eq!(
            click_state(
                &mut b,
                MouseButton::Right,
                (40, 11),
                t + Duration::from_secs(2)
            ),
            1
        );
    }
}
