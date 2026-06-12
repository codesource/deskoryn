//! Length-prefixed message framing.
//!
//! QUIC streams are byte streams, so we delimit messages ourselves: a 4-byte
//! big-endian length followed by the postcard-encoded body. Datagram channels
//! (audio) don't use this — a datagram is already a message boundary.

use bytes::{Buf, BufMut, BytesMut};
use serde::{de::DeserializeOwned, Serialize};

/// Reject absurd frames early (defense against a hostile/buggy peer).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeds max length: {0} > {max}", max = MAX_FRAME_LEN)]
    TooLarge(usize),
    #[error("encode error: {0}")]
    Encode(postcard::Error),
    #[error("decode error: {0}")]
    Decode(postcard::Error),
}

/// Encode `msg` into `out` as `[u32 len][postcard body]`.
pub fn encode<T: Serialize>(msg: &T, out: &mut BytesMut) -> Result<(), FrameError> {
    let body = postcard::to_allocvec(msg).map_err(FrameError::Encode)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(body.len()));
    }
    out.reserve(4 + body.len());
    out.put_u32(body.len() as u32);
    out.put_slice(&body);
    Ok(())
}

/// Encode a message for an unreliable **datagram** (no length prefix — the
/// datagram boundary is the message boundary). Used for audio frames.
pub fn to_datagram<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    postcard::to_allocvec(msg).map_err(FrameError::Encode)
}

/// Decode a message from a single datagram.
pub fn from_datagram<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    postcard::from_bytes(bytes).map_err(FrameError::Decode)
}

/// Try to decode one frame from the front of `buf`.
///
/// Returns `Ok(Some(msg))` and consumes the frame when a full one is buffered,
/// `Ok(None)` when more bytes are needed (leaving `buf` untouched), or an error
/// on a malformed/oversized frame.
pub fn decode_one<T: DeserializeOwned>(buf: &mut BytesMut) -> Result<Option<T>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    if buf.len() < 4 + len {
        return Ok(None); // wait for the rest
    }
    buf.advance(4);
    let body = buf.split_to(len);
    let msg = postcard::from_bytes(&body).map_err(FrameError::Decode)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ProtocolVersion;

    #[test]
    fn round_trip_and_partial() {
        let mut buf = BytesMut::new();
        let v = ProtocolVersion { major: 1, minor: 0 };
        encode(&v, &mut buf).unwrap();

        // Feed it one byte short, then complete it.
        let full = buf.split();
        let mut partial = BytesMut::from(&full[..full.len() - 1]);
        assert!(decode_one::<ProtocolVersion>(&mut partial).unwrap().is_none());
        partial.extend_from_slice(&full[full.len() - 1..]);
        let got: ProtocolVersion = decode_one(&mut partial).unwrap().unwrap();
        assert_eq!(got, v);
        assert!(partial.is_empty());
    }
}
