use std::fmt;

use serde::{Deserialize, Serialize};

/// Identity of a device: SHA-256 of its certificate's SubjectPublicKeyInfo (ADR-0004).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

impl DeviceId {
    /// Full identifier as lowercase RFC 4648 base32 without padding.
    pub fn to_base32(&self) -> String {
        base32(&self.0)
    }

    /// Parses the full 52-character base32 form (case-insensitive).
    pub fn from_base32(s: &str) -> Option<Self> {
        let bytes = unbase32(s)?;
        Some(Self(bytes.try_into().ok()?))
    }

    /// Short fingerprint shown to users: the first 8 base32 characters (40 bits).
    pub fn fingerprint(&self) -> String {
        let mut s = self.to_base32();
        s.truncate(8);
        s
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.fingerprint())
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fingerprint())
    }
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn unbase32(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c.to_ascii_lowercase() {
            b @ b'a'..=b'z' => b - b'a',
            b @ b'2'..=b'7' => b - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // Leftover bits must be padding zeros, otherwise the text is not canonical.
    (buffer & ((1 << bits) - 1) == 0).then_some(out)
}

/// Feature a message belongs to. Ranges: 0 core, 1..=255 built-in,
/// 256..=32767 future built-in, 32768.. plugins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FeatureId(pub u16);

impl FeatureId {
    pub const CORE: Self = Self(0);
    pub const INPUT: Self = Self(1);
    pub const CLIPBOARD: Self = Self(2);
    pub const FILES: Self = Self(3);
    pub const MIDI: Self = Self(4);
    pub const FIRST_PLUGIN: Self = Self(32768);

    pub fn is_plugin(self) -> bool {
        self.0 >= Self::FIRST_PLUGIN.0
    }
}

impl fmt::Debug for FeatureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FeatureId({self})")
    }
}

impl fmt::Display for FeatureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::CORE => f.write_str("core"),
            Self::INPUT => f.write_str("input"),
            Self::CLIPBOARD => f.write_str("clipboard"),
            Self::FILES => f.write_str("files"),
            Self::MIDI => f.write_str("midi"),
            Self(n) => write!(f, "#{n}"),
        }
    }
}

/// What a device allows a peer to do to it. Open set: new features add new ids.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct PermissionId(pub u16);

impl PermissionId {
    pub const INPUT: Self = Self(1);
    pub const FILES: Self = Self(2);
    pub const CLIPBOARD: Self = Self(3);
    pub const MIDI: Self = Self(4);
}

/// Operating system of a peer. Kept as an open code so that new platforms do not
/// break decoding on older peers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Platform(pub u8);

impl Platform {
    pub const UNKNOWN: Self = Self(0);
    pub const WINDOWS: Self = Self(1);
    pub const MACOS: Self = Self(2);
    pub const LINUX: Self = Self(3);
    pub const ANDROID: Self = Self(4);

    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::WINDOWS
        } else if cfg!(target_os = "macos") {
            Self::MACOS
        } else if cfg!(target_os = "android") {
            Self::ANDROID
        } else if cfg!(target_os = "linux") {
            Self::LINUX
        } else {
            Self::UNKNOWN
        }
    }
}

/// Opaque token that lets a session survive a change of connection (ADR-0002, ROADMAP).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SessionToken(pub [u8; 16]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc4648_vectors() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "my");
        assert_eq!(base32(b"fo"), "mzxq");
        assert_eq!(base32(b"foo"), "mzxw6");
        assert_eq!(base32(b"foob"), "mzxw6yq");
        assert_eq!(base32(b"fooba"), "mzxw6ytb");
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn base32_parses_back() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(unbase32(&base32(data)).as_deref(), Some(data));
        }
        let id = DeviceId([0x5a; 32]);
        assert_eq!(DeviceId::from_base32(&id.to_base32()), Some(id));
        assert_eq!(
            DeviceId::from_base32(&id.to_base32().to_uppercase()),
            Some(id)
        );
        assert_eq!(DeviceId::from_base32("abc1"), None);
        assert_eq!(DeviceId::from_base32("mzxw6"), None);
        assert_eq!(unbase32("mz"), None);
    }

    #[test]
    fn fingerprint_is_prefix_of_full_id() {
        let id = DeviceId([0xAB; 32]);
        assert_eq!(id.to_base32().len(), 52);
        assert!(id.to_base32().starts_with(&id.fingerprint()));
        assert_eq!(id.fingerprint().len(), 8);
        assert_eq!(format!("{id:?}"), format!("DeviceId({})", id.fingerprint()));
    }

    #[test]
    fn feature_ids_display_and_ranges() {
        assert_eq!(FeatureId::MIDI.to_string(), "midi");
        assert_eq!(FeatureId(700).to_string(), "#700");
        assert!(FeatureId(40000).is_plugin());
        assert!(!FeatureId::FILES.is_plugin());
    }

    #[test]
    fn current_platform_is_known_on_supported_targets() {
        assert_ne!(Platform::current(), Platform::UNKNOWN);
    }
}
