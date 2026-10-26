use crate::FeatureId;

/// Errors produced while decoding bytes received from a peer.
///
/// Every variant is recoverable: the caller decides whether to skip the message
/// or close the connection with [`crate::CloseCode::PROTOCOL_VIOLATION`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: u64, max: u32 },
    #[error("length prefix is not a valid varint")]
    BadLengthPrefix,
    #[error("input ended in the middle of a value")]
    Truncated,
    #[error("malformed payload: {0}")]
    Malformed(&'static str),
    #[error("{0} unexpected trailing bytes")]
    TrailingBytes(usize),
    #[error("unknown message kind {kind} for feature {feature}")]
    UnknownKind { feature: FeatureId, kind: u16 },
    #[error("unsupported datagram format {0}")]
    UnsupportedDatagram(u8),
    #[error("collection longer than {max} elements")]
    TooLong { max: usize },
    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("payload of {len} bytes exceeds the {max} byte limit")]
    TooLarge { len: usize, max: u32 },
    #[error("serialization failed: {0}")]
    Serialize(&'static str),
}

pub(crate) fn from_postcard(err: postcard::Error) -> DecodeError {
    match err {
        postcard::Error::DeserializeUnexpectedEnd => DecodeError::Truncated,
        postcard::Error::DeserializeBadVarint => DecodeError::Malformed("bad varint"),
        postcard::Error::DeserializeBadBool => DecodeError::Malformed("bad bool"),
        postcard::Error::DeserializeBadChar => DecodeError::Malformed("bad char"),
        postcard::Error::DeserializeBadUtf8 => DecodeError::Malformed("bad utf-8"),
        postcard::Error::DeserializeBadOption => DecodeError::Malformed("bad option tag"),
        postcard::Error::DeserializeBadEnum => DecodeError::Malformed("bad enum tag"),
        postcard::Error::DeserializeBadEncoding => DecodeError::Malformed("bad encoding"),
        postcard::Error::SerdeDeCustom => DecodeError::Malformed("value rejected"),
        _ => DecodeError::Malformed("postcard error"),
    }
}
