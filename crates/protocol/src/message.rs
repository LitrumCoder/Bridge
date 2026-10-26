use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{DecodeError, EncodeError, error::from_postcard};

/// Semantic checks that the type system cannot express. Runs on every decoded message.
pub trait Validate {
    fn validate(&self) -> Result<(), DecodeError> {
        Ok(())
    }
}

pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    postcard::to_allocvec(value).map_err(|_| EncodeError::Serialize("postcard"))
}

/// Strict decoding: the whole payload must be consumed and the value must validate.
pub fn decode_payload<T: DeserializeOwned + Validate>(payload: &[u8]) -> Result<T, DecodeError> {
    let (value, rest) = postcard::take_from_bytes::<T>(payload).map_err(from_postcard)?;
    if !rest.is_empty() {
        return Err(DecodeError::TrailingBytes(rest.len()));
    }
    value.validate()?;
    Ok(value)
}

/// Declares the message set of one feature: an enum with one variant per message type,
/// fixed wire `kind` numbers and dispatching encode/decode.
///
/// ```ignore
/// define_messages! {
///     pub enum ClipboardMessage for FeatureId::CLIPBOARD {
///         1 => Offer,
///         2 => Request,
///     }
/// }
/// ```
///
/// Published `kind` numbers and message layouts never change (ADR-0003).
#[macro_export]
macro_rules! define_messages {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident for $feature:path {
            $( $kind:literal => $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        $vis enum $name {
            $( $variant($variant), )+
        }

        impl $name {
            pub const FEATURE: $crate::FeatureId = $feature;
            pub const KINDS: &'static [u16] = &[$( $kind ),+];

            pub fn kind(&self) -> u16 {
                match self {
                    $( Self::$variant(_) => $kind, )+
                }
            }

            pub fn to_envelope(&self) -> ::std::result::Result<$crate::Envelope, $crate::EncodeError> {
                let payload = match self {
                    $( Self::$variant(m) => $crate::encode_payload(m)?, )+
                };
                Ok($crate::Envelope { feature: Self::FEATURE, kind: self.kind(), payload })
            }

            pub fn decode(kind: u16, payload: &[u8]) -> ::std::result::Result<Self, $crate::DecodeError> {
                match kind {
                    $( $kind => $crate::decode_payload::<$variant>(payload).map(Self::$variant), )+
                    _ => Err($crate::DecodeError::UnknownKind { feature: Self::FEATURE, kind }),
                }
            }

            pub fn from_envelope(env: &$crate::EnvelopeRef<'_>) -> ::std::result::Result<Self, $crate::DecodeError> {
                if env.feature != Self::FEATURE {
                    return Err($crate::DecodeError::Invalid("envelope belongs to another feature"));
                }
                Self::decode(env.kind, env.payload)
            }
        }

        $(
            impl ::std::convert::From<$variant> for $name {
                fn from(m: $variant) -> Self {
                    Self::$variant(m)
                }
            }
        )+
    };
}
