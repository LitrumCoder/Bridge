//! Receiver side: what each source injected, so it can all be released (ADR-0005), and
//! modifier remapping for macOS targets.

use std::collections::{HashMap, HashSet};

use bridge_protocol::DeviceId;
use bridge_protocol::input::{HidUsage, InputMessage, LayoutTag, LockState, Motion, MouseButton};

#[derive(Clone, PartialEq, Debug)]
pub enum InjectAction {
    MoveAbs { screen: u32, x: i32, y: i32 },
    MoveRel { dx: i32, dy: i32 },
    Key { usage: HidUsage, down: bool },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    Text(String),
    SetLayout(LayoutTag),
    SetLocks(LockState),
}

/// Ctrl ↔ Cmd swap when a non-Mac controls a Mac (configurable per pair).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Remap {
    pub swap_ctrl_gui: bool,
}

impl Remap {
    pub fn apply(self, k: HidUsage) -> HidUsage {
        if !self.swap_ctrl_gui {
            return k;
        }
        match k {
            HidUsage::LEFT_CTRL => HidUsage::LEFT_GUI,
            HidUsage::LEFT_GUI => HidUsage::LEFT_CTRL,
            HidUsage::RIGHT_CTRL => HidUsage::RIGHT_GUI,
            HidUsage::RIGHT_GUI => HidUsage::RIGHT_CTRL,
            other => other,
        }
    }
}

#[derive(Default, Debug)]
struct Source {
    active: bool,
    keys: HashSet<HidUsage>,
    buttons: HashSet<MouseButton>,
    last_seq: Option<u32>,
}

#[derive(Default, Debug)]
pub struct Injected {
    sources: HashMap<DeviceId, Source>,
}

impl Injected {
    pub fn on_message(
        &mut self,
        from: DeviceId,
        msg: &InputMessage,
        remap: Remap,
    ) -> Vec<InjectAction> {
        let src = self.sources.entry(from).or_default();
        match msg {
            InputMessage::Enter(e) => {
                src.active = true;
                let mut out = vec![
                    InjectAction::MoveAbs {
                        screen: e.screen,
                        x: e.x,
                        y: e.y,
                    },
                    InjectAction::SetLocks(e.locks),
                ];
                if let Some(l) = &e.layout {
                    out.push(InjectAction::SetLayout(l.clone()));
                }
                out
            }
            InputMessage::Leave(_) => {
                let out = Self::release(src);
                src.active = false;
                out
            }
            InputMessage::ReleaseAll(_) => Self::release(src),
            InputMessage::Key(k) => {
                let usage = remap.apply(k.usage);
                if k.down {
                    if !src.active {
                        return Vec::new();
                    }
                    src.keys.insert(usage);
                    vec![InjectAction::Key { usage, down: true }]
                } else if src.keys.remove(&usage) {
                    vec![InjectAction::Key { usage, down: false }]
                } else {
                    // Never inject a release we did not press: it could cut a key the
                    // local user is holding.
                    Vec::new()
                }
            }
            InputMessage::Button(b) => {
                if b.down {
                    if !src.active {
                        return Vec::new();
                    }
                    src.buttons.insert(b.button);
                    vec![InjectAction::Button {
                        button: b.button,
                        down: true,
                    }]
                } else if src.buttons.remove(&b.button) {
                    vec![InjectAction::Button {
                        button: b.button,
                        down: false,
                    }]
                } else {
                    Vec::new()
                }
            }
            InputMessage::Wheel(w) if src.active => {
                vec![InjectAction::Wheel { dx: w.dx, dy: w.dy }]
            }
            InputMessage::SetLayout(l) if src.active => {
                vec![InjectAction::SetLayout(l.layout.clone())]
            }
            InputMessage::Text(t) if src.active => vec![InjectAction::Text(t.text.to_string())],
            InputMessage::Wheel(_) | InputMessage::SetLayout(_) | InputMessage::Text(_) => {
                Vec::new()
            }
        }
    }

    /// Drops reordered (older) datagrams; absolute moves only while the source is active.
    pub fn on_motion(&mut self, from: DeviceId, m: Motion) -> Option<InjectAction> {
        let src = self.sources.entry(from).or_default();
        if src.last_seq.is_some_and(|last| !m.is_newer_than(last)) {
            return None;
        }
        src.last_seq = Some(m.seq());
        match m {
            Motion::Absolute { screen, x, y, .. } if src.active => {
                Some(InjectAction::MoveAbs { screen, x, y })
            }
            Motion::Relative { dx, dy, .. } => Some(InjectAction::MoveRel { dx, dy }),
            Motion::Absolute { .. } => None,
        }
    }

    /// The source is gone (disconnect, pause, leave): release everything it pressed.
    pub fn source_gone(&mut self, from: &DeviceId) -> Vec<InjectAction> {
        match self.sources.remove(from) {
            Some(mut src) => Self::release(&mut src),
            None => Vec::new(),
        }
    }

    pub fn pressed_count(&self) -> usize {
        self.sources
            .values()
            .map(|s| s.keys.len() + s.buttons.len())
            .sum()
    }

    fn release(src: &mut Source) -> Vec<InjectAction> {
        let mut keys: Vec<HidUsage> = src.keys.drain().collect();
        keys.sort();
        let mut out: Vec<InjectAction> = keys
            .into_iter()
            .map(|usage| InjectAction::Key { usage, down: false })
            .collect();
        out.extend(src.buttons.drain().map(|button| InjectAction::Button {
            button,
            down: false,
        }));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_protocol::input::{Button, Enter, Key, Leave, ReleaseAll};

    fn enter() -> InputMessage {
        Enter {
            screen: 0,
            x: 1,
            y: 2,
            locks: LockState::default(),
            layout: None,
        }
        .into()
    }

    fn key(usage: HidUsage, down: bool) -> InputMessage {
        Key {
            usage,
            down,
            repeat: false,
        }
        .into()
    }

    #[test]
    fn releases_only_what_was_pressed() {
        let a = DeviceId([1; 32]);
        let mut inj = Injected::default();
        assert!(
            inj.on_message(a, &key(HidUsage::A, true), Remap::default())
                .is_empty(),
            "no input before Enter"
        );
        inj.on_message(a, &enter(), Remap::default());
        inj.on_message(a, &key(HidUsage::A, true), Remap::default());
        inj.on_message(
            a,
            &Button {
                button: MouseButton::Left,
                down: true,
            }
            .into(),
            Remap::default(),
        );
        assert!(
            inj.on_message(a, &key(HidUsage::Q, false), Remap::default())
                .is_empty(),
            "stray release"
        );
        assert_eq!(inj.pressed_count(), 2);
        let out = inj.on_message(a, &ReleaseAll.into(), Remap::default());
        assert_eq!(out.len(), 2);
        assert_eq!(inj.pressed_count(), 0);
        inj.on_message(a, &key(HidUsage::LEFT_SHIFT, true), Remap::default());
        assert_eq!(
            inj.on_message(a, &Leave.into(), Remap::default()),
            vec![InjectAction::Key {
                usage: HidUsage::LEFT_SHIFT,
                down: false
            }]
        );
        inj.on_message(a, &enter(), Remap::default());
        inj.on_message(a, &key(HidUsage(0x05), true), Remap::default());
        assert_eq!(inj.source_gone(&a).len(), 1);
        assert!(inj.source_gone(&a).is_empty());
    }

    #[test]
    fn remap_swaps_ctrl_and_cmd_consistently() {
        let a = DeviceId([1; 32]);
        let mut inj = Injected::default();
        let swap = Remap {
            swap_ctrl_gui: true,
        };
        inj.on_message(a, &enter(), swap);
        assert_eq!(
            inj.on_message(a, &key(HidUsage::LEFT_CTRL, true), swap),
            vec![InjectAction::Key {
                usage: HidUsage::LEFT_GUI,
                down: true
            }]
        );
        assert_eq!(
            inj.on_message(a, &key(HidUsage::LEFT_CTRL, false), swap),
            vec![InjectAction::Key {
                usage: HidUsage::LEFT_GUI,
                down: false
            }]
        );
        assert_eq!(swap.apply(HidUsage::RIGHT_GUI), HidUsage::RIGHT_CTRL);
        assert_eq!(swap.apply(HidUsage::A), HidUsage::A);
    }

    #[test]
    fn motion_drops_old_datagrams_and_needs_enter() {
        let a = DeviceId([1; 32]);
        let mut inj = Injected::default();
        let abs = |seq| Motion::Absolute {
            seq,
            screen: 0,
            x: seq as i32,
            y: 0,
        };
        assert!(inj.on_motion(a, abs(1)).is_none(), "not entered yet");
        inj.on_message(a, &enter(), Remap::default());
        assert!(inj.on_motion(a, abs(3)).is_some());
        assert!(
            inj.on_motion(a, abs(2)).is_none(),
            "reordered older datagram"
        );
        assert!(inj.on_motion(a, abs(3)).is_none(), "duplicate");
        assert!(
            inj.on_motion(
                a,
                Motion::Relative {
                    seq: 4,
                    dx: 1,
                    dy: 1
                }
            )
            .is_some()
        );
    }
}
