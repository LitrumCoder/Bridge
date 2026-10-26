//! Messages of the shared cursor and keyboard (`FeatureId::INPUT`, ADR-0005).
//!
//! Keystrokes are sensitive: `Debug` of key and text messages never shows their content,
//! so they cannot leak into logs.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{BoundedString, DecodeError, FeatureId, Validate, define_messages};

/// USB HID usage on the keyboard page (0x07): the physical key, not the character.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HidUsage(pub u16);

impl fmt::Debug for HidUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_modifier() {
            write!(f, "HidUsage({:#04x})", self.0)
        } else {
            f.write_str("HidUsage(<key>)")
        }
    }
}

impl HidUsage {
    pub const A: Self = Self(0x04);
    pub const L: Self = Self(0x0F);
    pub const Q: Self = Self(0x14);
    pub const ENTER: Self = Self(0x28);
    pub const ESCAPE: Self = Self(0x29);
    pub const BACKSPACE: Self = Self(0x2A);
    pub const TAB: Self = Self(0x2B);
    pub const SPACE: Self = Self(0x2C);
    pub const CAPS_LOCK: Self = Self(0x39);
    pub const F1: Self = Self(0x3A);
    pub const F12: Self = Self(0x45);
    pub const NUM_LOCK: Self = Self(0x53);
    pub const LEFT_CTRL: Self = Self(0xE0);
    pub const LEFT_SHIFT: Self = Self(0xE1);
    pub const LEFT_ALT: Self = Self(0xE2);
    pub const LEFT_GUI: Self = Self(0xE3);
    pub const RIGHT_CTRL: Self = Self(0xE4);
    pub const RIGHT_SHIFT: Self = Self(0xE5);
    pub const RIGHT_ALT: Self = Self(0xE6);
    pub const RIGHT_GUI: Self = Self(0xE7);

    pub const fn is_modifier(self) -> bool {
        self.0 >= 0xE0 && self.0 <= 0xE7
    }

    /// Largest usage we accept from peers (keyboard page goes up to 0xE7, plus 0xFF).
    pub const MAX: u16 = 0xFF;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct LockState {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

/// Keyboard layout of the source, so the target types the same characters
/// (language sync, ARCHITECTURE §8.1).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LayoutTag {
    /// BCP 47 language, e.g. `ru-RU`.
    pub lang: BoundedString<16>,
    /// OS-specific layout id (KLID on Windows, input source id on macOS, XKB layout on Linux).
    pub id: BoundedString<64>,
}

/// The cursor arrives on the receiver, at a pixel of one of its screens.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Enter {
    pub screen: u32,
    pub x: i32,
    pub y: i32,
    pub locks: LockState,
    pub layout: Option<LayoutTag>,
}

/// The cursor left the receiver; it must not inject anything from this source until
/// the next `Enter` (pressed keys were already released with `ReleaseAll`).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Leave;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub usage: HidUsage,
    pub down: bool,
    pub repeat: bool,
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("usage", &self.usage)
            .field("down", &self.down)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Button {
    pub button: MouseButton,
    pub down: bool,
}

/// Wheel in 1/120 of a notch per axis (hi-res wheels send fractions).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Wheel {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ReleaseAll;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SetLayout {
    pub layout: LayoutTag,
}

/// Text typed on a phone keyboard, injected as characters rather than keys.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: BoundedString<256>,
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Text(<{} bytes>)", self.text.len())
    }
}

define_messages! {
    pub enum InputMessage for FeatureId::INPUT {
        1 => Enter,
        2 => Leave,
        3 => Key,
        4 => Button,
        5 => Wheel,
        6 => ReleaseAll,
        7 => SetLayout,
        8 => Text,
    }
}

/// Wheel deltas above this (≈ 100 notches in one event) are rejected.
pub const MAX_WHEEL: i32 = 12_000;
/// Pixel coordinates are bounded by the protocol's screen size limit.
pub const MAX_COORD: i32 = 1 << 20;

impl Validate for Enter {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.x.abs() > MAX_COORD || self.y.abs() > MAX_COORD {
            return Err(DecodeError::Invalid("enter position out of range"));
        }
        Ok(())
    }
}

impl Validate for Key {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.usage.0 > HidUsage::MAX || (self.repeat && !self.down) {
            return Err(DecodeError::Invalid("bad key event"));
        }
        Ok(())
    }
}

impl Validate for Wheel {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.dx.abs() > MAX_WHEEL || self.dy.abs() > MAX_WHEEL {
            return Err(DecodeError::Invalid("wheel delta out of range"));
        }
        Ok(())
    }
}

impl Validate for Leave {}
impl Validate for Button {}
impl Validate for ReleaseAll {}
impl Validate for SetLayout {}

impl Validate for Text {
    fn validate(&self) -> Result<(), DecodeError> {
        if self
            .text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(DecodeError::Invalid("control characters in text"));
        }
        Ok(())
    }
}

/// Payload of an `InputMotion` datagram. Absolute positions make a lost datagram
/// harmless; `seq` lets the receiver drop reordered, older ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Motion {
    /// Pixel on one of the receiver's screens (normal cursor sharing).
    Absolute {
        seq: u32,
        screen: u32,
        x: i32,
        y: i32,
    },
    /// Relative movement in receiver pixels (phone touchpad).
    Relative { seq: u32, dx: i32, dy: i32 },
}

impl Motion {
    pub fn seq(&self) -> u32 {
        match *self {
            Self::Absolute { seq, .. } | Self::Relative { seq, .. } => seq,
        }
    }

    /// True if `self` is newer than `last` with wrap-around (serial number arithmetic).
    pub fn is_newer_than(&self, last: u32) -> bool {
        let d = self.seq().wrapping_sub(last);
        d != 0 && d < u32::MAX / 2
    }
}

impl Validate for Motion {
    fn validate(&self) -> Result<(), DecodeError> {
        let ok = match *self {
            Self::Absolute { x, y, .. } => x.abs() <= MAX_COORD && y.abs() <= MAX_COORD,
            Self::Relative { dx, dy, .. } => dx.abs() <= MAX_COORD && dy.abs() <= MAX_COORD,
        };
        if ok {
            Ok(())
        } else {
            Err(DecodeError::Invalid("motion out of range"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, decode_payload, encode_payload};

    #[test]
    fn keystrokes_never_show_in_debug() {
        let k = Key {
            usage: HidUsage::A,
            down: true,
            repeat: false,
        };
        assert!(!format!("{k:?}").contains("0x04"));
        let t = Text {
            text: BoundedString::truncating("пароль"),
        };
        assert!(!format!("{t:?}").contains("пароль"));
        assert!(format!("{:?}", HidUsage::LEFT_CTRL).contains("0xe0"));
    }

    #[test]
    fn validation_rejects_out_of_range_values() {
        let m = |msg: InputMessage| {
            let env = msg.to_envelope().expect("encode");
            InputMessage::from_envelope(&Envelope::parse(&env.to_bytes()).expect("parse"))
        };
        assert!(
            m(Key {
                usage: HidUsage(0x300),
                down: true,
                repeat: false
            }
            .into())
            .is_err()
        );
        assert!(
            m(Key {
                usage: HidUsage::A,
                down: false,
                repeat: true
            }
            .into())
            .is_err()
        );
        assert!(
            m(Wheel {
                dx: 0,
                dy: MAX_WHEEL + 1
            }
            .into())
            .is_err()
        );
        assert!(
            m(Enter {
                screen: 0,
                x: MAX_COORD + 1,
                y: 0,
                locks: LockState::default(),
                layout: None
            }
            .into())
            .is_err()
        );
        assert!(
            m(Text {
                text: BoundedString::truncating("a\u{1b}b")
            }
            .into())
            .is_err()
        );
        assert!(
            m(Text {
                text: BoundedString::truncating("строка\n")
            }
            .into())
            .is_ok()
        );
        assert!(m(ReleaseAll.into()).is_ok());
    }

    #[test]
    fn motion_sequence_wraps_around() {
        let at = |seq| Motion::Relative { seq, dx: 1, dy: 0 };
        assert!(at(5).is_newer_than(4));
        assert!(!at(4).is_newer_than(5));
        assert!(!at(4).is_newer_than(4));
        assert!(at(1).is_newer_than(u32::MAX - 2), "wrap-around");
        let bytes = encode_payload(&Motion::Absolute {
            seq: 7,
            screen: 1,
            x: -5,
            y: 900,
        })
        .expect("encode");
        assert!(
            bytes.len() <= 12,
            "motion datagram stays tiny: {} bytes",
            bytes.len()
        );
        let back: Motion = decode_payload(&bytes).expect("decode");
        assert_eq!(back.seq(), 7);
        assert!(
            decode_payload::<Motion>(
                &encode_payload(&Motion::Relative {
                    seq: 1,
                    dx: MAX_COORD + 1,
                    dy: 0
                })
                .expect("e")
            )
            .is_err()
        );
    }
}
