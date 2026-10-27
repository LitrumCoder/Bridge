//! InputCapture: barriers on the outer edges; when the pointer hits one the compositor
//! "activates" the session and sends all input to us over EIS (as a receiver) until we
//! release it. The activation becomes a push across the edge for the router; while
//! active, EIS events are the grabbed stream.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::Session;
use ashpd::desktop::input_capture::{
    ActivatedBarrier, Barrier, Capabilities, ConnectToEISOptions, CreateSessionOptions,
    EnableOptions, GetZonesOptions, InputCapture as Portal, ReleaseOptions,
    SetPointerBarriersOptions,
};
use bridge_protocol::input::MouseButton;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{Edge, Rect, WaylandScreens, outer_barriers, to_screens};
use crate::keymap::evdev_to_hid;
use crate::{CaptureEvent, CaptureSink, Decision, InputCapture, PlatformError};

pub(crate) enum Cmd {
    Ungrab,
    Warp(i32, i32),
}

#[derive(Default)]
pub(crate) struct Shared {
    sink: Mutex<Option<Arc<dyn CaptureSink>>>,
}

impl Shared {
    fn decide(&self, ev: CaptureEvent) -> Decision {
        let sink = self.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
        sink.map_or(Decision::Pass, |s| s.on_event(ev))
    }
}

pub struct WaylandCapture {
    tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl WaylandCapture {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Cmd>) -> Self {
        Self {
            tx,
            shared: Arc::default(),
        }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

impl InputCapture for WaylandCapture {
    fn start(&self, sink: Arc<dyn CaptureSink>) -> Result<(), PlatformError> {
        *self.shared.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        Ok(())
    }

    fn set_grab(&self, grab: bool) {
        // Taking over happens in the compositor at the barrier; we only hand back.
        if !grab {
            let _ = self.tx.send(Cmd::Ungrab);
        }
    }

    fn warp(&self, x: i32, y: i32) {
        let _ = self.tx.send(Cmd::Warp(x, y));
    }

    fn stop(&self) {
        *self.shared.sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
        let _ = self.tx.send(Cmd::Ungrab);
    }
}

pub(crate) fn button(code: u32) -> Option<MouseButton> {
    Some(match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        0x113 => MouseButton::Back,
        0x114 => MouseButton::Forward,
        _ => return None,
    })
}

/// EIS scroll: discrete in 1/120 notches, positive down; the wire's positive is up.
pub(crate) fn wheel_from_discrete(dx: i32, dy: i32) -> (i32, i32) {
    (dx, -dy)
}

/// Smooth scroll in logical pixels, 10 px per notch.
pub(crate) fn wheel_from_pixels(dx: f32, dy: f32) -> (i32, i32) {
    ((dx * 12.0).round() as i32, (-dy * 12.0).round() as i32)
}

struct Active {
    id: Option<u32>,
    park: (i32, i32),
}

struct Capture<'a> {
    portal: &'a Portal,
    session: Session<Portal>,
    shared: Arc<Shared>,
    screens: Arc<WaylandScreens>,
    barriers: HashMap<u32, Edge>,
    active: Option<Active>,
    pressed: HashSet<u32>,
    rest: (f32, f32),
}

impl Capture<'_> {
    async fn arm(&mut self) -> Result<(), ashpd::Error> {
        let zones = self
            .portal
            .zones(&self.session, GetZonesOptions::default())
            .await?
            .response()?;
        let rects: Vec<Rect> = zones
            .regions()
            .iter()
            .map(|r| Rect {
                x: r.x_offset(),
                y: r.y_offset(),
                w: r.width() as i32,
                h: r.height() as i32,
            })
            .collect();
        *self.screens.zones.lock().unwrap_or_else(|p| p.into_inner()) = to_screens(&rects, |_| 1.0);
        self.barriers.clear();
        let mut list = Vec::new();
        for (n, (edge, x1, y1, x2, y2)) in outer_barriers(&rects).into_iter().enumerate() {
            let id = n as u32 + 1;
            let Some(nz) = NonZeroU32::new(id) else {
                continue;
            };
            self.barriers.insert(id, edge);
            list.push(Barrier::new(nz, (x1, y1, x2, y2)));
        }
        let resp = self
            .portal
            .set_pointer_barriers(
                &self.session,
                &list,
                zones.zone_set(),
                SetPointerBarriersOptions::default(),
            )
            .await?
            .response()?;
        if !resp.failed_barriers().is_empty() {
            warn!(
                failed = resp.failed_barriers().len(),
                "the compositor rejected some edge barriers"
            );
        }
        self.portal
            .enable(&self.session, EnableOptions::default())
            .await
    }

    async fn release(&mut self, at: Option<(i32, i32)>) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.pressed.clear();
        let mut opts = ReleaseOptions::default().set_activation_id(active.id);
        if let Some((x, y)) = at {
            opts = opts.set_cursor_position((f64::from(x), f64::from(y)));
        }
        if let Err(e) = self.portal.release(&self.session, opts).await {
            warn!("cannot release the input capture: {e}");
        }
    }

    async fn on_activated(
        &mut self,
        id: Option<u32>,
        pos: (f32, f32),
        barrier: Option<ActivatedBarrier>,
    ) {
        let (x, y) = (pos.0.round() as i32, pos.1.round() as i32);
        let edge = match barrier {
            Some(ActivatedBarrier::Barrier(b)) => self.barriers.get(&b.get()).copied(),
            _ => None,
        };
        self.active = Some(Active { id, park: (x, y) });
        self.rest = (0.0, 0.0);
        let crossed = edge.is_some_and(|edge| {
            let (dx, dy) = edge.outward();
            self.shared.decide(CaptureEvent::Motion { x, y, dx, dy }) == Decision::Swallow
        });
        if !crossed {
            // Nothing on that side: give the pointer back where it was.
            let back = edge.map_or((x, y), |e| {
                let (dx, dy) = e.outward();
                (x - dx, y - dy)
            });
            self.release(Some(back)).await;
        }
    }

    fn on_ei(&mut self, ev: &EiEvent) {
        let Some(active) = &self.active else { return };
        let (px, py) = active.park;
        let out = match ev {
            EiEvent::PointerMotion(m) => {
                let fx = self.rest.0 + m.dx;
                let fy = self.rest.1 + m.dy;
                let (dx, dy) = (fx.trunc(), fy.trunc());
                self.rest = (fx - dx, fy - dy);
                (dx != 0.0 || dy != 0.0).then_some(CaptureEvent::Motion {
                    x: px,
                    y: py,
                    dx: dx as i32,
                    dy: dy as i32,
                })
            }
            EiEvent::Button(b) => button(b.button).map(|button| CaptureEvent::Button {
                button,
                down: b.state == ei::button::ButtonState::Press,
            }),
            EiEvent::ScrollDiscrete(s) => {
                let (dx, dy) = wheel_from_discrete(s.discrete_dx, s.discrete_dy);
                Some(CaptureEvent::Wheel { dx, dy })
            }
            EiEvent::ScrollDelta(s) => {
                let (dx, dy) = wheel_from_pixels(s.dx, s.dy);
                (dx != 0 || dy != 0).then_some(CaptureEvent::Wheel { dx, dy })
            }
            EiEvent::KeyboardKey(k) => {
                let down = k.state == ei::keyboard::KeyState::Press;
                let repeat = if down {
                    !self.pressed.insert(k.key)
                } else {
                    self.pressed.remove(&k.key);
                    false
                };
                u16::try_from(k.key)
                    .ok()
                    .and_then(evdev_to_hid)
                    .map(|usage| CaptureEvent::Key {
                        usage,
                        down,
                        repeat,
                    })
            }
            _ => None,
        };
        if let Some(e) = out {
            self.shared.decide(e);
        }
    }
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    screens: Arc<WaylandScreens>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let portal = Portal::new().await.map_err(|e| s(&e))?;
    let (session, caps) = portal
        .create_session(
            None,
            CreateSessionOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await
        .map_err(|e| s(&e))?;
    info!(?caps, "input capture session created");
    let fd = portal
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .map_err(|e| s(&e))?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(|e| s(&e))?;
    let (_conn, mut events) = context
        .handshake_tokio("bridge", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| s(&e))?;
    let mut activated = portal.receive_activated().await.map_err(|e| s(&e))?;
    let mut deactivated = portal.receive_deactivated().await.map_err(|e| s(&e))?;
    let mut zones_changed = portal.receive_zones_changed().await.map_err(|e| s(&e))?;
    let mut disabled = portal.receive_disabled().await.map_err(|e| s(&e))?;
    let mut c = Capture {
        portal: &portal,
        session,
        shared,
        screens,
        barriers: HashMap::new(),
        active: None,
        pressed: HashSet::new(),
        rest: (0.0, 0.0),
    };
    c.arm().await.map_err(|e| s(&e))?;
    loop {
        tokio::select! {
            Some(a) = activated.next() => {
                let pos = a.cursor_position().unwrap_or((0.0, 0.0));
                c.on_activated(a.activation_id(), pos, a.barrier_id()).await;
            }
            Some(d) = deactivated.next() => {
                // The compositor took input back (e.g. the user pressed its escape key).
                // TODO(M4c+): tell the router the pointer is local again.
                debug!(id = ?d.activation_id(), "input capture deactivated");
                c.active = None;
                c.pressed.clear();
            }
            Some(_) = zones_changed.next() => {
                if let Err(e) = c.arm().await {
                    warn!("cannot re-arm the edge barriers: {e}");
                }
            }
            Some(_) = disabled.next() => {
                c.active = None;
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(e) = c.arm().await {
                    warn!("input capture was disabled and cannot be re-enabled: {e}");
                }
            }
            ev = events.next() => match ev {
                Some(Ok(EiEvent::SeatAdded(seat))) => {
                    seat.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Scroll
                            | DeviceCapability::Button,
                    );
                    let _ = context.flush();
                }
                Some(Ok(EiEvent::Disconnected(_))) | None => return Err("EIS disconnected".into()),
                Some(Ok(ev)) => c.on_ei(&ev),
                Some(Err(e)) => return Err(s(&e)),
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Ungrab) => {
                    // The router follows Grab(false) with a Warp to the return point.
                    let at = match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                        Ok(Some(Cmd::Warp(x, y))) => Some((x, y)),
                        _ => None,
                    };
                    c.release(at).await;
                }
                Some(Cmd::Warp(..)) => {}
                None => {
                    c.release(None).await;
                    return Ok(());
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eis_codes_and_wheel() {
        assert_eq!(button(0x110), Some(MouseButton::Left));
        assert_eq!(button(0x114), Some(MouseButton::Forward));
        assert_eq!(button(0x115), None);
        assert_eq!(wheel_from_discrete(0, 120), (0, -120));
        assert_eq!(wheel_from_pixels(10.0, -10.0), (120, 120));
    }
}
