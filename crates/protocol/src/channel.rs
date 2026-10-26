use std::fmt;

use serde::{Deserialize, Serialize};

/// Logical channel of a peer connection (ARCHITECTURE §3.1). The core scheduler maps
/// classes to connections and priorities; the protocol only fixes their wire limits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ChannelClass {
    Control,
    InputMotion,
    InputEvents,
    Midi,
    Clipboard,
    Bulk,
}

impl ChannelClass {
    pub const ALL: [ChannelClass; 6] = [
        Self::Control,
        Self::InputMotion,
        Self::InputEvents,
        Self::Midi,
        Self::Clipboard,
        Self::Bulk,
    ];

    /// Largest frame body accepted on this channel. For `InputMotion` this is the
    /// datagram payload limit, for `Bulk` the transfer header (file bytes are raw).
    pub const fn max_frame_len(self) -> u32 {
        match self {
            Self::Control => 64 * 1024,
            Self::InputMotion => 1024,
            Self::InputEvents => 4 * 1024,
            Self::Midi => 72 * 1024,
            Self::Clipboard => 1024 * 1024,
            Self::Bulk => 4 * 1024,
        }
    }

    pub const fn is_datagram(self) -> bool {
        matches!(self, Self::InputMotion)
    }
}

/// QUIC application close code (ARCHITECTURE §3.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CloseCode(pub u32);

impl CloseCode {
    pub const NORMAL: Self = Self(0);
    pub const NOT_PAIRED: Self = Self(1);
    pub const INCOMPATIBLE_VERSION: Self = Self(2);
    pub const REVOKED: Self = Self(3);
    pub const LEAVING: Self = Self(4);
    pub const PROTOCOL_VIOLATION: Self = Self(5);
    pub const PAIRING_FAILED: Self = Self(6);
    pub const RATE_LIMITED: Self = Self(7);

    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::NORMAL => "normal",
            Self::NOT_PAIRED => "not paired",
            Self::INCOMPATIBLE_VERSION => "incompatible version",
            Self::REVOKED => "revoked",
            Self::LEAVING => "leaving",
            Self::PROTOCOL_VIOLATION => "protocol violation",
            Self::PAIRING_FAILED => "pairing failed",
            Self::RATE_LIMITED => "rate limited",
            _ => return None,
        })
    }
}

impl fmt::Debug for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => write!(f, "CloseCode({n})"),
            None => write!(f, "CloseCode({})", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_limits_are_small_and_bulk_header_is_bounded() {
        for c in ChannelClass::ALL {
            assert!(c.max_frame_len() <= 1024 * 1024, "{c:?}");
        }
        assert!(ChannelClass::InputMotion.is_datagram());
        assert!(!ChannelClass::Bulk.is_datagram());
    }

    #[test]
    fn close_codes_have_names() {
        assert_eq!(format!("{:?}", CloseCode::LEAVING), "CloseCode(leaving)");
        assert_eq!(format!("{:?}", CloseCode(99)), "CloseCode(99)");
    }
}
