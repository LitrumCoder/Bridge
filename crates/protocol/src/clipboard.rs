//! Messages of the shared clipboard (`FeatureId::CLIPBOARD`, ADR-0007).
//!
//! The owner of a copy sends an `Offer`: small text formats travel inline, the rest is
//! fetched with `Request` when someone pastes and arrives as `Data` chunks. Contents
//! never show in `Debug`.

use serde::{Deserialize, Serialize};

use crate::{BoundedBytes, BoundedVec, DecodeError, FeatureId, Validate, define_messages};

/// Clipboard format on the wire. An open code: formats a peer does not know are
/// skipped, not rejected.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ClipFormat(pub u8);

impl ClipFormat {
    /// UTF-8, `\n` line ends.
    pub const TEXT: Self = Self(1);
    /// UTF-8 HTML (a fragment or a document).
    pub const HTML: Self = Self(2);
    pub const RTF: Self = Self(3);
    pub const PNG: Self = Self(4);
    /// A file list, served by the files engine (milestone 6a).
    pub const FILES: Self = Self(5);

    pub const KNOWN: [Self; 5] = [Self::TEXT, Self::HTML, Self::RTF, Self::PNG, Self::FILES];

    pub fn is_known(self) -> bool {
        Self::KNOWN.contains(&self)
    }
}

/// Inline contents of one offer, all formats together.
pub const INLINE_LIMIT: usize = 256 * 1024;
/// Largest `Data` chunk; frames on the clipboard channel are at most 1 MiB.
pub const CHUNK_LIMIT: usize = 512 * 1024;
/// Largest single format we transfer at all.
pub const FORMAT_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Offered {
    pub format: ClipFormat,
    /// Exact size when known, 0 when the owner cannot tell without rendering.
    pub size: u64,
    pub inline: Option<BoundedBytes<INLINE_LIMIT>>,
}

/// The sender's clipboard changed. Replaces any earlier offer from the same sender.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub seq: u64,
    pub formats: BoundedVec<Offered, 8>,
}

/// Asks the owner of offer `seq` for one format.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Request {
    pub seq: u64,
    pub format: ClipFormat,
}

/// A piece of a requested format: bytes `offset..offset + bytes.len()` of `total`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Data {
    pub seq: u64,
    pub format: ClipFormat,
    pub offset: u64,
    pub total: u64,
    pub bytes: BoundedBytes<CHUNK_LIMIT>,
}

/// The requested format is gone (the clipboard changed again, or reading failed).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Unavailable {
    pub seq: u64,
    pub format: ClipFormat,
}

define_messages! {
    pub enum ClipboardMessage for FeatureId::CLIPBOARD {
        1 => Offer,
        2 => Request,
        3 => Data,
        4 => Unavailable,
    }
}

impl Validate for Offer {
    fn validate(&self) -> Result<(), DecodeError> {
        let mut inline = 0usize;
        for (i, f) in self.formats.iter().enumerate() {
            if self.formats[..i].iter().any(|g| g.format == f.format) {
                return Err(DecodeError::Invalid("duplicate clipboard format"));
            }
            if f.size > FORMAT_LIMIT {
                return Err(DecodeError::Invalid("clipboard format too large"));
            }
            if let Some(data) = &f.inline {
                if data.len() as u64 != f.size {
                    return Err(DecodeError::Invalid("inline size mismatch"));
                }
                inline += data.len();
            }
        }
        if inline > INLINE_LIMIT {
            return Err(DecodeError::Invalid("inline clipboard data over the limit"));
        }
        Ok(())
    }
}

impl Validate for Request {}
impl Validate for Unavailable {}

impl Validate for Data {
    fn validate(&self) -> Result<(), DecodeError> {
        let end = self.offset.checked_add(self.bytes.len() as u64);
        if self.total > FORMAT_LIMIT || end.is_none_or(|e| e > self.total) {
            return Err(DecodeError::Invalid("clipboard chunk out of range"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, EnvelopeRef};

    fn roundtrip(m: ClipboardMessage) -> Result<ClipboardMessage, DecodeError> {
        let bytes = m.to_envelope().expect("encode").to_bytes();
        let env: EnvelopeRef<'_> = Envelope::parse(&bytes).expect("parse");
        ClipboardMessage::from_envelope(&env)
    }

    fn inline(format: ClipFormat, data: &[u8]) -> Offered {
        Offered {
            format,
            size: data.len() as u64,
            inline: BoundedBytes::new(data.to_vec()),
        }
    }

    #[test]
    fn offers_validate_sizes_and_duplicates() {
        let ok = Offer {
            seq: 1,
            formats: BoundedVec::try_from_vec(vec![
                inline(ClipFormat::TEXT, b"hi"),
                Offered {
                    format: ClipFormat::PNG,
                    size: 5_000_000,
                    inline: None,
                },
            ])
            .expect("fits"),
        };
        assert_eq!(roundtrip(ok.clone().into()).expect("valid"), ok.into());

        let dup = Offer {
            seq: 2,
            formats: BoundedVec::try_from_vec(vec![
                inline(ClipFormat::TEXT, b"a"),
                inline(ClipFormat::TEXT, b"b"),
            ])
            .expect("fits"),
        };
        assert!(roundtrip(dup.into()).is_err());

        let mut lying = inline(ClipFormat::HTML, b"<b>x</b>");
        lying.size = 3;
        let bad = Offer {
            seq: 3,
            formats: BoundedVec::try_from_vec(vec![lying]).expect("fits"),
        };
        assert!(roundtrip(bad.into()).is_err());

        let half = vec![0u8; INLINE_LIMIT / 2 + 1];
        let over = Offer {
            seq: 4,
            formats: BoundedVec::try_from_vec(vec![
                inline(ClipFormat::TEXT, &half),
                inline(ClipFormat::HTML, &half),
            ])
            .expect("fits"),
        };
        assert!(roundtrip(over.into()).is_err(), "inline total is bounded");
    }

    #[test]
    fn chunks_stay_inside_the_total() {
        let chunk = |offset, total, n| Data {
            seq: 1,
            format: ClipFormat::PNG,
            offset,
            total,
            bytes: BoundedBytes::new(vec![7; n]).expect("fits"),
        };
        assert!(roundtrip(chunk(0, 10, 10).into()).is_ok());
        assert!(roundtrip(chunk(5, 10, 6).into()).is_err());
        assert!(roundtrip(chunk(u64::MAX, u64::MAX, 1).into()).is_err());
        assert!(roundtrip(chunk(0, FORMAT_LIMIT + 1, 1).into()).is_err());
        assert!(format!("{:?}", chunk(0, 3, 3)).contains("<3 bytes>"));
        assert!(ClipFormat::PNG.is_known() && !ClipFormat(99).is_known());
        assert!(
            roundtrip(
                Request {
                    seq: 9,
                    format: ClipFormat(99)
                }
                .into()
            )
            .is_ok(),
            "unknown formats decode"
        );
    }
}
