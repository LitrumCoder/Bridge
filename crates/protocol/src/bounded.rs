//! Collections whose length is checked while decoding, before untrusted input can
//! make us allocate more than the protocol allows.

use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T, const N: usize> BoundedVec<T, N> {
    pub const MAX: usize = N;

    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Returns the vector back if it is longer than `N`.
    pub fn try_from_vec(v: Vec<T>) -> Result<Self, Vec<T>> {
        if v.len() <= N { Ok(Self(v)) } else { Err(v) }
    }

    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.0.len() < N {
            self.0.push(value);
            Ok(())
        } else {
            Err(value)
        }
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for BoundedVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize, const N: usize> Serialize for BoundedVec<T, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedVec<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T, const N: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for V<T, N> {
            type Value = BoundedVec<T, N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a sequence of at most {N} elements")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let hint = seq.size_hint().unwrap_or(0);
                if hint > N {
                    return Err(de::Error::invalid_length(hint, &self));
                }
                let mut out = Vec::with_capacity(hint);
                while let Some(item) = seq.next_element()? {
                    if out.len() == N {
                        return Err(de::Error::invalid_length(N + 1, &self));
                    }
                    out.push(item);
                }
                Ok(BoundedVec(out))
            }
        }

        d.deserialize_seq(V::<T, N>(PhantomData))
    }
}

/// Opaque bytes of at most `N`, encoded as one length-prefixed blob. `Debug` shows only
/// the length: payloads like clipboard contents must never reach the logs.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct BoundedBytes<const N: usize>(Vec<u8>);

impl<const N: usize> BoundedBytes<N> {
    pub const MAX: usize = N;

    pub fn new(v: Vec<u8>) -> Option<Self> {
        (v.len() <= N).then_some(Self(v))
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl<const N: usize> Deref for BoundedBytes<N> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedBytes<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

impl<const N: usize> Serialize for BoundedBytes<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedBytes<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const N: usize>;

        impl<const N: usize> Visitor<'_> for V<N> {
            type Value = BoundedBytes<N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {N} bytes")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                if v.len() > N {
                    return Err(E::invalid_length(v.len(), &self));
                }
                Ok(BoundedBytes(v.to_vec()))
            }
        }

        d.deserialize_bytes(V::<N>)
    }
}

/// UTF-8 string of at most `N` bytes.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct BoundedString<const N: usize>(String);

impl<const N: usize> BoundedString<N> {
    pub const MAX: usize = N;

    pub fn new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        (s.len() <= N).then_some(Self(s))
    }

    /// Cuts the string at the last character boundary that fits into `N` bytes.
    pub fn truncating(s: &str) -> Self {
        let mut end = s.len().min(N);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        Self(s[..end].to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> Deref for BoundedString<N> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<const N: usize> fmt::Display for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<const N: usize> Serialize for BoundedString<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedString<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const N: usize>;

        impl<const N: usize> Visitor<'_> for V<N> {
            type Value = BoundedString<N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a string of at most {N} bytes")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.len() > N {
                    return Err(E::invalid_length(v.len(), &self));
                }
                Ok(BoundedString(v.to_owned()))
            }
        }

        d.deserialize_str(V::<N>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_respects_limit_on_construction_and_push() {
        assert!(BoundedVec::<u8, 2>::try_from_vec(vec![1, 2, 3]).is_err());
        let mut v = BoundedVec::<u8, 2>::try_from_vec(vec![1]).unwrap();
        assert!(v.try_push(2).is_ok());
        assert_eq!(v.try_push(3), Err(3));
        assert_eq!(&*v, &[1, 2]);
        assert_eq!(v.into_vec(), vec![1, 2]);
    }

    #[test]
    fn vec_decoding_rejects_declared_length_over_limit() {
        let bytes = postcard::to_allocvec(&vec![1u8, 2, 3]).unwrap();
        assert!(postcard::from_bytes::<BoundedVec<u8, 2>>(&bytes).is_err());
        let ok: BoundedVec<u8, 3> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(&*ok, &[1, 2, 3]);
    }

    #[test]
    fn huge_declared_length_does_not_allocate_or_panic() {
        // varint u32::MAX followed by nothing
        let bytes = [0xff, 0xff, 0xff, 0xff, 0x0f];
        assert!(postcard::from_bytes::<BoundedVec<u64, 16>>(&bytes).is_err());
    }

    #[test]
    fn string_limit_counts_bytes() {
        assert!(BoundedString::<4>::new("абв").is_none());
        assert!(BoundedString::<6>::new("абв").is_some());
        let bytes = postcard::to_allocvec("абв").unwrap();
        assert!(postcard::from_bytes::<BoundedString<4>>(&bytes).is_err());
        let s: BoundedString<6> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(s.as_str(), "абв");
    }

    #[test]
    fn bytes_are_one_blob_and_hidden_in_debug() {
        let b = BoundedBytes::<4>::new(b"abc".to_vec()).unwrap();
        let enc = postcard::to_allocvec(&b).unwrap();
        assert_eq!(enc, vec![3, b'a', b'b', b'c']);
        assert_eq!(postcard::from_bytes::<BoundedBytes<4>>(&enc).unwrap(), b);
        assert!(postcard::from_bytes::<BoundedBytes<2>>(&enc).is_err());
        assert!(BoundedBytes::<2>::new(vec![0; 3]).is_none());
        assert_eq!(format!("{b:?}"), "<3 bytes>");
        assert_eq!(b.into_vec(), b"abc");
    }

    #[test]
    fn truncating_keeps_char_boundaries() {
        assert_eq!(BoundedString::<3>::truncating("абв").as_str(), "а");
        assert_eq!(
            BoundedString::<64>::truncating("Desktop-PC").as_str(),
            "Desktop-PC"
        );
    }
}
