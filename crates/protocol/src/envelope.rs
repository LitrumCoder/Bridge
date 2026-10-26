//! `Envelope = varint(feature) ‖ varint(kind) ‖ payload` — the payload runs to the end
//! of the frame, so the core can route without parsing it.

use crate::frame::{read_varint, write_varint};
use crate::{DecodeError, FeatureId};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Envelope {
    pub feature: FeatureId,
    pub kind: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EnvelopeRef<'a> {
    pub feature: FeatureId,
    pub kind: u16,
    pub payload: &'a [u8],
}

impl Envelope {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 6);
        write_varint(self.feature.0.into(), &mut out);
        write_varint(self.kind.into(), &mut out);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn parse(bytes: &[u8]) -> Result<EnvelopeRef<'_>, DecodeError> {
        let (feature, rest) = read_u16(bytes)?;
        let (kind, payload) = read_u16(rest)?;
        Ok(EnvelopeRef {
            feature: FeatureId(feature),
            kind,
            payload,
        })
    }
}

impl EnvelopeRef<'_> {
    pub fn to_owned(&self) -> Envelope {
        Envelope {
            feature: self.feature,
            kind: self.kind,
            payload: self.payload.to_vec(),
        }
    }
}

fn read_u16(bytes: &[u8]) -> Result<(u16, &[u8]), DecodeError> {
    let (v, used) = read_varint(bytes)?.ok_or(DecodeError::Truncated)?;
    let v = u16::try_from(v).map_err(|_| DecodeError::Malformed("header field exceeds u16"))?;
    Ok((v, &bytes[used..]))
}

pub const DATAGRAM_FORMAT: u8 = 1;

/// `[format u8] ‖ varint(feature) ‖ payload`.
pub fn encode_datagram(feature: FeatureId, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(DATAGRAM_FORMAT);
    write_varint(feature.0.into(), &mut out);
    out.extend_from_slice(payload);
    out
}

pub fn decode_datagram(bytes: &[u8]) -> Result<(FeatureId, &[u8]), DecodeError> {
    let (&format, rest) = bytes.split_first().ok_or(DecodeError::Truncated)?;
    if format != DATAGRAM_FORMAT {
        return Err(DecodeError::UnsupportedDatagram(format));
    }
    let (feature, payload) = read_u16(rest)?;
    Ok((FeatureId(feature), payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_and_header_size() {
        let e = Envelope {
            feature: FeatureId::MIDI,
            kind: 300,
            payload: vec![1, 2, 3],
        };
        let bytes = e.to_bytes();
        assert_eq!(bytes.len(), 1 + 2 + 3);
        assert_eq!(Envelope::parse(&bytes).unwrap().to_owned(), e);
    }

    #[test]
    fn envelope_rejects_truncated_and_oversized_header() {
        assert_eq!(Envelope::parse(&[]), Err(DecodeError::Truncated));
        assert_eq!(Envelope::parse(&[0x01]), Err(DecodeError::Truncated));
        assert!(Envelope::parse(&[0xff, 0xff, 0x04, 0x00]).is_err());
    }

    #[test]
    fn datagram_roundtrip_and_unknown_format() {
        let d = encode_datagram(FeatureId::INPUT, &[9, 9]);
        assert_eq!(
            decode_datagram(&d).unwrap(),
            (FeatureId::INPUT, &[9u8, 9][..])
        );
        assert_eq!(
            decode_datagram(&[2, 1]),
            Err(DecodeError::UnsupportedDatagram(2))
        );
        assert_eq!(decode_datagram(&[]), Err(DecodeError::Truncated));
    }
}
