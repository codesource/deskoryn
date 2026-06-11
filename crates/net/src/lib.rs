//! # deskoryn-net
//!
//! Everything between the OS-feature modules and the wire: secure session
//! transport, LAN discovery, and pairing.
//!
//! The crate is split into a **trait surface** (always compiled, pure Rust) and
//! **real implementations** behind feature flags:
//!
//! * [`transport`] — the [`Session`] abstraction (channels of typed messages).
//!   `default` ships an in-memory loopback used by tests; enable `quic` for the
//!   real [`quic`] backend (QUIC + TLS 1.3 via quinn/rustls).
//! * [`discovery`] — [`Discovery`] trait; enable `mdns` for the real backend.
//! * [`pairing`] — short-authentication-string verification and the trust flow.
//!
//! This keeps `cargo check` fast and OS-portable while the production transport
//! is a flag away. See `docs/PROTOCOL.md` and `docs/SECURITY.md`.

pub mod discovery;
pub mod pairing;
pub mod transport;

#[cfg(feature = "quic")]
pub mod quic;

pub use discovery::{Discovery, PeerHint};
pub use pairing::{PairingError, PairingSession, ShortAuthString};
pub use transport::{Session, SessionError, Sink, Source};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Pairing(#[from] PairingError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
