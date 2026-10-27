//! Source side of the shared cursor (ARCHITECTURE §4.3). Runs synchronously inside the
//! OS input hook: every event gets an immediate pass/swallow decision, messages for
//! peers are queued for the network task.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use bridge_protocol::DeviceId;
use bridge_protocol::input::{
    Button, Enter, HidUsage, InputMessage, Key, LayoutTag, Leave, LockState, Motion, MouseButton,
    ReleaseAll, Wheel,
};

use super::geometry::{CursorPos, Layout};
use super::keys::{Dest, HotkeyAction, Hotkeys, KeyTracker};
use super::shake::ShakeDetector;
use crate::Micros;

/// If the network task stops draining actions for this long while we hold the
/// keyboard, give control back to this device (never lose the machine).
pub const WATCHDOG: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptureEvent {
    /// `(x, y)`: cursor position in the local virtual desktop before the move;
    /// `(dx, dy)`: the move in local pixels. In grab mode only the delta matters.
    Motion {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Key {
        usage: HidUsage,
        down: bool,
        repeat: bool,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Let the local OS process the event.
    Pass,
    /// Hide the event from the local OS.
    Swallow,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReturnReason {
    Edge,
    Hotkey,
    Emergency,
    PeerUnavailable,
    Watchdog,
}

#[derive(Clone, PartialEq, Debug)]
pub enum RouterEvent {
    Entered {
        peer: DeviceId,
    },
    Returned {
        reason: ReturnReason,
    },
    Locked(bool),
    /// The user shook the mouse; show where the cursor is.
    Shake {
        device: DeviceId,
    },
}

#[derive(Clone, PartialEq, Debug)]
pub enum RouterAction {
    Send {
        peer: DeviceId,
        msg: InputMessage,
    },
    Motion {
        peer: DeviceId,
        motion: Motion,
    },
    /// Switch the capture between observing (false) and grabbing (true).
    Grab(bool),
    /// Put the local cursor at this virtual-desktop pixel.
    Warp {
        x: i32,
        y: i32,
    },
    Event(RouterEvent),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    Local,
    Remote { peer: DeviceId, pos: CursorPos },
}

pub struct InputRouter {
    me: DeviceId,
    layout: Layout,
    target: Target,
    available: HashSet<DeviceId>,
    locked: bool,
    keys: KeyTracker,
    hotkeys: Hotkeys,
    shake: ShakeDetector,
    actions: VecDeque<RouterAction>,
    seq: u32,
    last_local: (i32, i32),
    locks: LockState,
    layout_tag: Option<LayoutTag>,
    drag_active: bool,
    /// A file drag crossed to this peer: the button pressed here is held there too,
    /// and its release goes to both (ADR-0008).
    dragging: Option<DeviceId>,
    last_drain: Micros,
}

impl InputRouter {
    pub fn new(me: DeviceId, layout: Layout, hotkeys: Hotkeys) -> Self {
        Self {
            me,
            layout,
            target: Target::Local,
            available: HashSet::new(),
            locked: false,
            keys: KeyTracker::default(),
            hotkeys,
            shake: ShakeDetector::default(),
            actions: VecDeque::new(),
            seq: 0,
            last_local: (0, 0),
            locks: LockState::default(),
            layout_tag: None,
            drag_active: false,
            dragging: None,
            last_drain: Micros::ZERO,
        }
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
        if let Target::Remote { pos, .. } = self.target
            && self.layout.screen(pos.screen).is_none()
        {
            self.return_local(ReturnReason::PeerUnavailable);
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Peers we may control: connected, allowing us input, not paused.
    pub fn set_available(&mut self, peer: DeviceId, available: bool) {
        if available {
            self.available.insert(peer);
            return;
        }
        self.available.remove(&peer);
        self.keys.drop_peer(peer);
        if matches!(self.target, Target::Remote { peer: p, .. } if p == peer) {
            self.target = Target::Local;
            self.push(RouterAction::Grab(false));
            let (x, y) = self.last_local;
            self.push(RouterAction::Warp { x, y });
            self.push(RouterAction::Event(RouterEvent::Returned {
                reason: ReturnReason::PeerUnavailable,
            }));
        }
    }

    pub fn set_locks(&mut self, locks: LockState) {
        self.locks = locks;
    }

    pub fn set_layout_tag(&mut self, tag: Option<LayoutTag>) {
        self.layout_tag = tag;
    }

    /// A file drag is in progress: crossing with a button held is allowed (milestone 6b).
    pub fn set_drag_active(&mut self, active: bool) {
        self.drag_active = active;
    }

    pub fn current_peer(&self) -> Option<DeviceId> {
        match self.target {
            Target::Remote { peer, .. } => Some(peer),
            Target::Local => None,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn has_actions(&self) -> bool {
        !self.actions.is_empty()
    }

    /// Hands out queued actions to the network task (also feeds the watchdog).
    pub fn poll_action(&mut self, now: Micros) -> Option<RouterAction> {
        self.last_drain = now;
        self.actions.pop_front()
    }

    pub fn on_event(&mut self, now: Micros, ev: CaptureEvent) -> Decision {
        if matches!(self.target, Target::Remote { .. })
            && !self.actions.is_empty()
            && now.saturating_sub(self.last_drain) > WATCHDOG
        {
            self.actions.clear();
            self.return_local(ReturnReason::Watchdog);
        }
        match ev {
            CaptureEvent::Key {
                usage,
                down: true,
                repeat: false,
            } => self.key_down(usage),
            CaptureEvent::Key {
                usage,
                down: true,
                repeat: true,
            } => self.key_repeat(usage),
            CaptureEvent::Key {
                usage, down: false, ..
            } => self.key_up(usage),
            CaptureEvent::Button { button, down } => self.button(button, down),
            CaptureEvent::Wheel { dx, dy } => match self.target {
                Target::Local => Decision::Pass,
                Target::Remote { peer, .. } => {
                    self.send(peer, Wheel { dx, dy }.into());
                    Decision::Swallow
                }
            },
            CaptureEvent::Motion { x, y, dx, dy } => self.motion(now, x, y, dx, dy),
        }
    }

    fn current_dest(&self) -> Dest {
        match self.target {
            Target::Local => Dest::Local,
            Target::Remote { peer, .. } => Dest::Peer(peer),
        }
    }

    fn key_down(&mut self, usage: HidUsage) -> Decision {
        if let Some(action) = self.hotkeys.lookup(self.keys.mods(), usage) {
            self.keys.press_key(usage, Dest::Dropped);
            self.hotkey(action);
            return Decision::Swallow;
        }
        let dest = self.current_dest();
        self.keys.press_key(usage, dest);
        self.deliver_key(dest, usage, true, false)
    }

    fn key_repeat(&mut self, usage: HidUsage) -> Decision {
        let dest = match self.keys.key_dest(usage) {
            Some(d) => d,
            None => {
                let d = self.current_dest();
                self.keys.press_key(usage, d);
                d
            }
        };
        self.deliver_key(dest, usage, true, true)
    }

    fn key_up(&mut self, usage: HidUsage) -> Decision {
        match self.keys.release_key(usage) {
            // Unknown key: it was pressed before we started; the OS may hold it.
            None => Decision::Pass,
            Some(dest) => self.deliver_key(dest, usage, false, false),
        }
    }

    fn deliver_key(&mut self, dest: Dest, usage: HidUsage, down: bool, repeat: bool) -> Decision {
        match dest {
            Dest::Local => Decision::Pass,
            Dest::Dropped => Decision::Swallow,
            Dest::Peer(p) => {
                if self.available.contains(&p) {
                    self.send(
                        p,
                        Key {
                            usage,
                            down,
                            repeat,
                        }
                        .into(),
                    );
                }
                Decision::Swallow
            }
        }
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Decision {
        let dest = if down {
            let d = self.current_dest();
            self.keys.press_button(button, d);
            d
        } else {
            match self.keys.release_button(button) {
                None => return Decision::Pass,
                Some(d) => d,
            }
        };
        if !down
            && button == MouseButton::Left
            && dest == Dest::Local
            && let Some(p) = self.dragging.take()
        {
            // The drop happens over there; the local drag loop ends here too, in our
            // own window, where it does nothing.
            if self.available.contains(&p) {
                self.send(
                    p,
                    Button {
                        button,
                        down: false,
                    }
                    .into(),
                );
            }
            return Decision::Pass;
        }
        match dest {
            Dest::Local => Decision::Pass,
            Dest::Dropped => Decision::Swallow,
            Dest::Peer(p) => {
                if self.available.contains(&p) {
                    self.send(p, Button { button, down }.into());
                }
                Decision::Swallow
            }
        }
    }

    fn present_fn(&self) -> impl Fn(&DeviceId) -> bool + '_ {
        move |d: &DeviceId| *d == self.me || self.available.contains(d)
    }

    fn motion(&mut self, now: Micros, x: i32, y: i32, dx: i32, dy: i32) -> Decision {
        match self.target {
            Target::Local => {
                if self.locked {
                    return Decision::Pass;
                }
                let Some(pos) = self.local_pos(x, y) else {
                    return Decision::Pass;
                };
                let next = self
                    .layout
                    .step(pos, f64::from(dx), f64::from(dy), &self.present_fn());
                if next.screen.device == self.me {
                    return Decision::Pass;
                }
                if self.keys.any_button() && !self.drag_active {
                    // Crossing while dragging something that is not a file would leave
                    // the button held on the wrong machine.
                    return Decision::Pass;
                }
                self.last_local = (x, y);
                self.enter(next.screen.device, next);
                Decision::Swallow
            }
            Target::Remote { peer, pos } => {
                if self.dragging == Some(peer) {
                    // Files are on their way to this peer: the cursor stays until the drop.
                    let only = |d: &DeviceId| *d == peer;
                    let (sx, sy) = self.scale_to(pos);
                    let next = self
                        .layout
                        .step(pos, f64::from(dx) * sx, f64::from(dy) * sy, &only);
                    let moved = next.pixel() != pos.pixel() || next.screen != pos.screen;
                    self.target = Target::Remote { peer, pos: next };
                    if moved {
                        self.send_motion(peer, next);
                    }
                    return Decision::Swallow;
                }
                if self.shake.feed(now, f64::from(dx)) {
                    self.push(RouterAction::Event(RouterEvent::Shake { device: peer }));
                }
                let (sx, sy) = self.scale_to(pos);
                let next = if self.locked {
                    let only = |d: &DeviceId| *d == peer;
                    self.layout
                        .step(pos, f64::from(dx) * sx, f64::from(dy) * sy, &only)
                } else {
                    self.layout.step(
                        pos,
                        f64::from(dx) * sx,
                        f64::from(dy) * sy,
                        &self.present_fn(),
                    )
                };
                if next.screen.device == self.me {
                    self.leave_peer(peer);
                    self.target = Target::Local;
                    self.push(RouterAction::Grab(false));
                    if let Some(s) = self.layout.screen(next.screen) {
                        let (px, py) = next.pixel();
                        self.push(RouterAction::Warp {
                            x: s.origin_x + px,
                            y: s.origin_y + py,
                        });
                    }
                    self.push(RouterAction::Event(RouterEvent::Returned {
                        reason: ReturnReason::Edge,
                    }));
                } else if next.screen.device != peer {
                    self.leave_peer(peer);
                    self.enter(next.screen.device, next);
                } else {
                    let moved = next.pixel() != pos.pixel() || next.screen != pos.screen;
                    self.target = Target::Remote { peer, pos: next };
                    if moved {
                        self.send_motion(peer, next);
                    }
                }
                Decision::Swallow
            }
        }
    }

    /// Ratio converting source pixels into the current screen's pixels at equal mm.
    fn scale_to(&self, pos: CursorPos) -> (f64, f64) {
        let src = self.layout.device_screens(self.me).next().map(|s| s.ppmm());
        let dst = self.layout.screen(pos.screen).map(|s| s.ppmm());
        match (src, dst) {
            (Some((sx, sy)), Some((dx, dy))) => (dx / sx, dy / sy),
            _ => (1.0, 1.0),
        }
    }

    fn local_pos(&self, x: i32, y: i32) -> Option<CursorPos> {
        let s = self.layout.device_screens(self.me).find(|s| {
            let (lx, ly) = (x - s.origin_x, y - s.origin_y);
            lx >= 0 && ly >= 0 && (lx as u32) < s.px_w && (ly as u32) < s.px_h
        })?;
        Some(CursorPos {
            screen: s.r#ref(),
            x: f64::from(x - s.origin_x) + 0.5,
            y: f64::from(y - s.origin_y) + 0.5,
        })
    }

    fn enter(&mut self, peer: DeviceId, pos: CursorPos) {
        if matches!(self.target, Target::Local) {
            self.push(RouterAction::Grab(true));
        }
        self.target = Target::Remote { peer, pos };
        let (x, y) = pos.pixel();
        let enter = Enter {
            screen: pos.screen.screen,
            x,
            y,
            locks: self.locks,
            layout: self.layout_tag.clone(),
        };
        self.send(peer, enter.into());
        self.send_motion(peer, pos);
        if self.drag_active && self.keys.button_dest(MouseButton::Left) == Some(Dest::Local) {
            self.send(
                peer,
                Button {
                    button: MouseButton::Left,
                    down: true,
                }
                .into(),
            );
            self.dragging = Some(peer);
        }
        self.push(RouterAction::Event(RouterEvent::Entered { peer }));
    }

    fn leave_peer(&mut self, peer: DeviceId) {
        if self.dragging == Some(peer) {
            self.dragging = None;
        }
        self.send(peer, ReleaseAll.into());
        self.send(peer, Leave.into());
        self.keys.drop_peer(peer);
    }

    fn return_local(&mut self, reason: ReturnReason) {
        if let Target::Remote { peer, .. } = self.target {
            if self.available.contains(&peer) {
                self.leave_peer(peer);
            } else {
                self.keys.drop_peer(peer);
            }
            self.target = Target::Local;
            self.push(RouterAction::Grab(false));
            let (x, y) = self.last_local;
            self.push(RouterAction::Warp { x, y });
            self.push(RouterAction::Event(RouterEvent::Returned { reason }));
        }
    }

    fn hotkey(&mut self, action: HotkeyAction) {
        match action {
            HotkeyAction::ToggleLock => {
                self.locked = !self.locked;
                self.push(RouterAction::Event(RouterEvent::Locked(self.locked)));
            }
            HotkeyAction::EmergencyLocal => {
                self.locked = false;
                self.return_local(ReturnReason::Emergency);
            }
            HotkeyAction::Goto(n) => {
                let Some(&device) = self.layout.devices_in_order().get(n) else {
                    return;
                };
                if device == self.me {
                    self.return_local(ReturnReason::Hotkey);
                    return;
                }
                if !self.available.contains(&device) || self.current_peer() == Some(device) {
                    return;
                }
                let Some(s) = self.layout.device_screens(device).next() else {
                    return;
                };
                let (cx, cy) = s.center();
                let pos = CursorPos {
                    screen: s.r#ref(),
                    x: cx,
                    y: cy,
                };
                if let Target::Remote { peer, .. } = self.target {
                    self.leave_peer(peer);
                }
                self.enter(device, pos);
            }
        }
    }

    fn send(&mut self, peer: DeviceId, msg: InputMessage) {
        self.push(RouterAction::Send { peer, msg });
    }

    fn send_motion(&mut self, peer: DeviceId, pos: CursorPos) {
        self.seq = self.seq.wrapping_add(1);
        let (x, y) = pos.pixel();
        let motion = Motion::Absolute {
            seq: self.seq,
            screen: pos.screen.screen,
            x,
            y,
        };
        self.push(RouterAction::Motion { peer, motion });
    }

    fn push(&mut self, a: RouterAction) {
        self.actions.push_back(a);
    }
}
