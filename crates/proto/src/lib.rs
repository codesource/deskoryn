//! # deskoryn-proto
//!
//! The Deskoryn wire protocol — the vocabulary two daemons speak over their
//! secure session. This crate is transport-agnostic: it defines *messages* and
//! *framing*, not sockets. `deskoryn-net` maps the logical channels below onto
//! QUIC streams / datagrams.
//!
//! ## Channels
//!
//! A session multiplexes several logical channels (see [`Channel`]):
//!
//! | Channel      | Transport mapping        | Reliability | Carries                          |
//! |--------------|--------------------------|-------------|----------------------------------|
//! | `Control`    | one bidi QUIC stream     | reliable    | handshake, heartbeat, layout     |
//! | `Input`      | one bidi QUIC stream     | reliable+ordered | input events (latency-critical) |
//! | `Clipboard`  | bidi stream + pull streams | reliable  | offers + on-demand payloads      |
//! | `FileXfer`   | one uni stream per transfer | reliable | manifests + chunks               |
//! | `Audio`      | QUIC datagrams           | unreliable  | Opus frames (drop-tolerant)      |
//!
//! Messages are encoded with [`postcard`] (compact, no_std-friendly, schema is
//! the Rust types) and length-prefixed by [`framing`].

pub mod framing;
pub mod message;

pub use framing::{decode_one, encode, from_datagram, to_datagram, FrameError};
pub use message::*;

/// Bump on any breaking change to the message schema. The handshake refuses to
/// proceed unless both peers agree on the major version.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// Logical channels multiplexed over one secure session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Control,
    Input,
    Clipboard,
    FileXfer,
    Audio,
}
