//! Where every pressed key went, so its release goes to the same place (ADR-0005), and
//! hotkey chords.

use std::collections::HashMap;

use bridge_protocol::DeviceId;
use bridge_protocol::input::{HidUsage, MouseButton};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dest {
    /// The press reached the local OS; its release must too.
    Local,
    Peer(DeviceId),
    /// The target already released it (switch, disconnect) or it was a hotkey:
    /// the release goes nowhere.
    Dropped,
}

#[derive(Default, Debug)]
pub(crate) struct KeyTracker {
    keys: HashMap<HidUsage, Dest>,
    buttons: HashMap<MouseButton, Dest>,
}

impl KeyTracker {
    pub(crate) fn press_key(&mut self, k: HidUsage, dest: Dest) {
        self.keys.insert(k, dest);
    }

    pub(crate) fn key_dest(&self, k: HidUsage) -> Option<Dest> {
        self.keys.get(&k).copied()
    }

    pub(crate) fn release_key(&mut self, k: HidUsage) -> Option<Dest> {
        self.keys.remove(&k)
    }

    pub(crate) fn press_button(&mut self, b: MouseButton, dest: Dest) {
        self.buttons.insert(b, dest);
    }

    pub(crate) fn release_button(&mut self, b: MouseButton) -> Option<Dest> {
        self.buttons.remove(&b)
    }

    pub(crate) fn any_button(&self) -> bool {
        !self.buttons.is_empty()
    }

    /// After `peer` released everything (switch, disconnect), its releases go nowhere.
    pub(crate) fn drop_peer(&mut self, peer: DeviceId) {
        for d in self.keys.values_mut().chain(self.buttons.values_mut()) {
            if *d == Dest::Peer(peer) {
                *d = Dest::Dropped;
            }
        }
    }

    pub(crate) fn mods(&self) -> Mods {
        Mods::from_keys(self.keys.keys().copied())
    }
}

/// Modifier state regardless of side (left or right).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub gui: bool,
}

impl Mods {
    pub const CTRL_ALT: Self = Self {
        ctrl: true,
        alt: true,
        shift: false,
        gui: false,
    };
    pub const CTRL_ALT_SHIFT: Self = Self {
        ctrl: true,
        alt: true,
        shift: true,
        gui: false,
    };

    fn from_keys(keys: impl Iterator<Item = HidUsage>) -> Self {
        let mut m = Self::default();
        for k in keys {
            match k {
                HidUsage::LEFT_CTRL | HidUsage::RIGHT_CTRL => m.ctrl = true,
                HidUsage::LEFT_ALT | HidUsage::RIGHT_ALT => m.alt = true,
                HidUsage::LEFT_SHIFT | HidUsage::RIGHT_SHIFT => m.shift = true,
                HidUsage::LEFT_GUI | HidUsage::RIGHT_GUI => m.gui = true,
                _ => {}
            }
        }
        m
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotkeyAction {
    /// Keep the cursor on the current screen (game mode).
    ToggleLock,
    /// Jump to the n-th device in layout order (0-based).
    Goto(usize),
    /// Take control back to this device immediately.
    EmergencyLocal,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    pub mods: Mods,
    pub key: HidUsage,
}

#[derive(Clone, Debug)]
pub struct Hotkeys {
    pub bindings: Vec<(Chord, HotkeyAction)>,
}

impl Default for Hotkeys {
    /// Ctrl+Alt+L lock, Ctrl+Alt+F1…F9 go to device N, Ctrl+Alt+Shift+Esc emergency return.
    fn default() -> Self {
        let mut bindings = vec![
            (
                Chord {
                    mods: Mods::CTRL_ALT,
                    key: HidUsage::L,
                },
                HotkeyAction::ToggleLock,
            ),
            (
                Chord {
                    mods: Mods::CTRL_ALT_SHIFT,
                    key: HidUsage::ESCAPE,
                },
                HotkeyAction::EmergencyLocal,
            ),
        ];
        for n in 0..9u16 {
            bindings.push((
                Chord {
                    mods: Mods::CTRL_ALT,
                    key: HidUsage(HidUsage::F1.0 + n),
                },
                HotkeyAction::Goto(usize::from(n)),
            ));
        }
        Self { bindings }
    }
}

impl Hotkeys {
    pub fn lookup(&self, mods: Mods, key: HidUsage) -> Option<HotkeyAction> {
        self.bindings
            .iter()
            .find(|(c, _)| c.mods == mods && c.key == key)
            .map(|(_, a)| *a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_remembers_destinations_and_drops_peers() {
        let p = DeviceId([1; 32]);
        let mut t = KeyTracker::default();
        t.press_key(HidUsage::A, Dest::Peer(p));
        t.press_key(HidUsage::LEFT_SHIFT, Dest::Local);
        t.press_button(MouseButton::Left, Dest::Peer(p));
        assert!(t.any_button());
        t.drop_peer(p);
        assert_eq!(t.key_dest(HidUsage::A), Some(Dest::Dropped));
        assert_eq!(t.release_key(HidUsage::LEFT_SHIFT), Some(Dest::Local));
        assert_eq!(t.release_button(MouseButton::Left), Some(Dest::Dropped));
        assert!(!t.any_button());
        assert_eq!(t.release_key(HidUsage::Q), None);
    }

    #[test]
    fn default_hotkeys() {
        let h = Hotkeys::default();
        assert_eq!(
            h.lookup(Mods::CTRL_ALT, HidUsage::L),
            Some(HotkeyAction::ToggleLock)
        );
        assert_eq!(
            h.lookup(Mods::CTRL_ALT, HidUsage(HidUsage::F1.0 + 2)),
            Some(HotkeyAction::Goto(2))
        );
        assert_eq!(
            h.lookup(Mods::CTRL_ALT_SHIFT, HidUsage::ESCAPE),
            Some(HotkeyAction::EmergencyLocal)
        );
        assert_eq!(h.lookup(Mods::default(), HidUsage::L), None);
        let mut t = KeyTracker::default();
        t.press_key(HidUsage::RIGHT_CTRL, Dest::Local);
        t.press_key(HidUsage::LEFT_ALT, Dest::Local);
        assert_eq!(t.mods(), Mods::CTRL_ALT);
    }
}
