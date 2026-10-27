//! Messages of file transfers (`FeatureId::FILES`, ARCHITECTURE §8.3).
//!
//! Control messages go on the control stream; file bytes go on bulk-connection streams
//! that start with a [`BulkHeader`]. Every path component is validated here, at
//! decoding, and again by the engine before anything touches the disk.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{BoundedString, BoundedVec, DecodeError, FeatureId, Validate, define_messages};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TransferId(pub [u8; 16]);

impl fmt::Debug for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransferId(")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

pub type Component = BoundedString<255>;

/// Items in one transfer, directories included.
pub const MAX_ITEMS: u32 = 100_000;
pub const MAX_DEPTH: usize = 32;
pub const ITEMS_PER_MESSAGE: usize = 128;

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A file or directory name that is safe on every OS we write to: no separators,
/// no `.`/`..`, no drive or stream syntax, no control characters, no Windows device
/// names or trailing dots and spaces.
pub fn is_safe_component(c: &str) -> bool {
    if c.is_empty() || c == "." || c == ".." || c.ends_with('.') || c.ends_with(' ') {
        return false;
    }
    if c.chars().any(|ch| {
        ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return false;
    }
    let stem = c.split('.').next().unwrap_or(c).trim_end();
    !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ItemKind {
    File,
    Dir,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Item {
    pub path: BoundedVec<Component, MAX_DEPTH>,
    pub kind: ItemKind,
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub mtime_ms: i64,
}

impl Item {
    pub fn is_safe(&self) -> bool {
        !self.path.is_empty()
            && self.path.iter().all(|c| is_safe_component(c))
            && (self.kind == ItemKind::File || self.size == 0)
    }
}

/// Starts a transfer; the items follow in `Items` messages.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub transfer: TransferId,
    pub items: u32,
    pub bytes: u64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Items {
    pub transfer: TransferId,
    pub first: u32,
    pub items: BoundedVec<Item, ITEMS_PER_MESSAGE>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DeclineReason {
    NotAllowed,
    TooLarge,
    NoSpace,
    BadItems,
    Busy,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Decline {
    pub transfer: TransferId,
    pub reason: DeclineReason,
}

/// The receiver asks for item `item` from byte `offset` (resume) on a bulk stream.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Want {
    pub transfer: TransferId,
    pub item: u32,
    pub offset: u64,
}

/// After the last byte of an item: the hash of the whole file and the metadata it
/// was read with. A source that changed since the offer is reported with new values.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ItemDone {
    pub transfer: TransferId,
    pub item: u32,
    pub size: u64,
    pub mtime_ms: i64,
    pub blake3: [u8; 32],
}

/// The source of an item changed or vanished; the receiver restarts it (or skips it
/// when `size` is `None`).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Restart {
    pub transfer: TransferId,
    pub item: u32,
    pub size: Option<u64>,
    pub mtime_ms: i64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Cancel {
    pub transfer: TransferId,
}

/// The receiver has every item (`ok`) or gave up.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Finished {
    pub transfer: TransferId,
    pub ok: bool,
}

define_messages! {
    pub enum FilesMessage for FeatureId::FILES {
        1 => Offer,
        2 => Items,
        3 => Decline,
        4 => Want,
        5 => ItemDone,
        6 => Restart,
        7 => Cancel,
        8 => Finished,
    }
}

/// First bytes of every bulk stream (length-prefixed postcard), then raw file bytes.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct BulkHeader {
    pub transfer: TransferId,
    pub item: u32,
    pub offset: u64,
}

impl Validate for BulkHeader {}

impl Validate for Offer {
    fn validate(&self) -> Result<(), DecodeError> {
        if self.items == 0 || self.items > MAX_ITEMS {
            return Err(DecodeError::Invalid("bad item count"));
        }
        Ok(())
    }
}

impl Validate for Items {
    fn validate(&self) -> Result<(), DecodeError> {
        if !self.items.iter().all(Item::is_safe) {
            return Err(DecodeError::Invalid("unsafe file path"));
        }
        if self
            .first
            .checked_add(self.items.len() as u32)
            .is_none_or(|e| e > MAX_ITEMS)
        {
            return Err(DecodeError::Invalid("item index out of range"));
        }
        Ok(())
    }
}

impl Validate for Decline {}
impl Validate for Want {}
impl Validate for ItemDone {}
impl Validate for Restart {}
impl Validate for Cancel {}
impl Validate for Finished {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Envelope;

    #[test]
    fn unsafe_components_are_rejected() {
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:",
            "x:y",
            "nul",
            "COM1.txt",
            "lpt9",
            "aux .log",
            "trail.",
            "trail ",
            "tab\there",
            "q?",
            "<x>",
            "a|b",
            "\u{0}",
        ] {
            assert!(!is_safe_component(bad), "{bad:?}");
        }
        for ok in [
            "report.pdf",
            "Отчёт 2026.docx",
            ".hidden",
            "con-figure",
            "a..b",
            "COM10",
        ] {
            assert!(is_safe_component(ok), "{ok:?}");
        }
    }

    fn item(path: &[&str]) -> Item {
        Item {
            path: BoundedVec::try_from_vec(
                path.iter().map(|c| BoundedString::truncating(c)).collect(),
            )
            .expect("depth"),
            kind: ItemKind::File,
            size: 3,
            mtime_ms: 0,
        }
    }

    #[test]
    fn traversal_cannot_pass_decoding() {
        let decode = |items: Vec<Item>| {
            let m: FilesMessage = Items {
                transfer: TransferId([1; 16]),
                first: 0,
                items: BoundedVec::try_from_vec(items).expect("fits"),
            }
            .into();
            let bytes = m.to_envelope().expect("encode").to_bytes();
            FilesMessage::from_envelope(&Envelope::parse(&bytes).expect("parse"))
        };
        assert!(decode(vec![item(&["dir", "file.txt"])]).is_ok());
        assert!(decode(vec![item(&["..", "etc", "passwd"])]).is_err());
        assert!(decode(vec![item(&["dir", "..", "x"])]).is_err());
        assert!(decode(vec![item(&[])]).is_err());
        let mut dir = item(&["d"]);
        dir.kind = ItemKind::Dir;
        assert!(
            decode(vec![dir.clone()]).is_err(),
            "directories have no size"
        );
        dir.size = 0;
        assert!(decode(vec![dir]).is_ok());
    }

    #[test]
    fn offers_and_ids() {
        let bytes = FilesMessage::from(Offer {
            transfer: TransferId([0xab; 16]),
            items: 0,
            bytes: 0,
        })
        .to_envelope()
        .expect("encode")
        .to_bytes();
        assert!(FilesMessage::from_envelope(&Envelope::parse(&bytes).expect("p")).is_err());
        let id = TransferId([0xab; 16]);
        assert_eq!(format!("{id:?}"), "TransferId(abababab)");
        assert_eq!(id.to_string().len(), 32);
    }
}
