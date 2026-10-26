//! Stream framing: `varint(len) ‖ body`. The length is checked against the channel
//! limit as soon as the prefix is read, before the body is buffered.

use crate::DecodeError;

const MAX_VARINT_LEN: usize = 5;

pub fn encode_frame(
    body: &[u8],
    max_len: u32,
    out: &mut Vec<u8>,
) -> Result<(), crate::EncodeError> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&l| l <= max_len)
        .ok_or(crate::EncodeError::TooLarge {
            len: body.len(),
            max: max_len,
        })?;
    write_varint(len, out);
    out.extend_from_slice(body);
    Ok(())
}

pub(crate) fn write_varint(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Parses a LEB128 `u32`. `Ok(None)` means more bytes are needed.
pub(crate) fn read_varint(buf: &[u8]) -> Result<Option<(u32, usize)>, DecodeError> {
    let mut value: u64 = 0;
    for (i, &b) in buf.iter().enumerate().take(MAX_VARINT_LEN) {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return u32::try_from(value)
                .map(|v| Some((v, i + 1)))
                .map_err(|_| DecodeError::BadLengthPrefix);
        }
    }
    if buf.len() >= MAX_VARINT_LEN {
        Err(DecodeError::BadLengthPrefix)
    } else {
        Ok(None)
    }
}

/// Incremental decoder for one stream.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max_len: u32,
}

impl FrameDecoder {
    pub fn new(max_len: u32) -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            max_len,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Bytes received but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Returns the next complete frame. After an error the stream must be closed.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        let pending = &self.buf[self.start..];
        let Some((len, prefix)) = read_varint(pending)? else {
            return Ok(None);
        };
        if len > self.max_len {
            return Err(DecodeError::FrameTooLarge {
                len: len.into(),
                max: self.max_len,
            });
        }
        let end = prefix + len as usize;
        if pending.len() < end {
            return Ok(None);
        }
        let frame = pending[prefix..end].to_vec();
        self.start += end;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_edges() {
        for v in [0, 1, 127, 128, 16_383, 16_384, u32::MAX] {
            let mut out = Vec::new();
            write_varint(v, &mut out);
            assert_eq!(read_varint(&out).unwrap(), Some((v, out.len())));
            assert_eq!(read_varint(&out[..out.len() - 1]).unwrap(), None);
        }
    }

    #[test]
    fn varint_rejects_overlong_and_overflow() {
        assert_eq!(read_varint(&[0x80; 5]), Err(DecodeError::BadLengthPrefix));
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x1f]),
            Err(DecodeError::BadLengthPrefix)
        );
    }

    #[test]
    fn frames_survive_arbitrary_chunking() {
        let mut wire = Vec::new();
        let bodies: Vec<Vec<u8>> = vec![vec![], vec![7; 3], vec![1; 300]];
        for b in &bodies {
            encode_frame(b, 1024, &mut wire).unwrap();
        }
        for chunk in [1, 2, 7, wire.len()] {
            let mut dec = FrameDecoder::new(1024);
            let mut got = Vec::new();
            for piece in wire.chunks(chunk) {
                dec.push(piece);
                while let Some(f) = dec.next_frame().unwrap() {
                    got.push(f);
                }
            }
            assert_eq!(got, bodies);
            assert_eq!(dec.buffered(), 0);
        }
    }

    #[test]
    fn oversized_frame_is_rejected_before_body_arrives() {
        let mut dec = FrameDecoder::new(16);
        let mut prefix = Vec::new();
        write_varint(17, &mut prefix);
        dec.push(&prefix);
        assert_eq!(
            dec.next_frame(),
            Err(DecodeError::FrameTooLarge { len: 17, max: 16 })
        );
    }

    #[test]
    fn encoder_refuses_bodies_over_limit() {
        let mut out = Vec::new();
        assert!(encode_frame(&[0; 17], 16, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn buffer_is_compacted_while_streaming() {
        let mut dec = FrameDecoder::new(1024);
        let mut wire = Vec::new();
        encode_frame(&[5; 100], 1024, &mut wire).unwrap();
        for _ in 0..1000 {
            dec.push(&wire);
            assert!(dec.next_frame().unwrap().is_some());
        }
        assert!(dec.buf.capacity() < 10_000);
    }
}
