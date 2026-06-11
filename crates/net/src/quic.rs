//! QUIC + TLS 1.3 transport backend (feature = "quic").
//!
//! Channel → transport mapping (see `docs/PROTOCOL.md`):
//!
//! * Each reliable [`Channel`](deskoryn_proto::Channel) is one bidirectional
//!   QUIC stream opened on demand; messages are length-prefixed
//!   ([`deskoryn_proto::framing`]).
//! * The audio channel uses **QUIC datagrams** (unreliable, no head-of-line
//!   blocking) so a lost Opus packet never stalls input or clipboard traffic.
//! * Mutual authentication: both endpoints present self-signed certificates
//!   bound to their [`DeviceId`]; a custom `rustls` verifier accepts a peer only
//!   if its certificate fingerprint matches the pin in the trust store (or we
//!   are mid-pairing, in which case any cert is accepted and the SAS protects us).
//!
//! This module is a skeleton: signatures and wiring outline are present; the
//! bodies marked `todo!()` are where the quinn/rustls glue goes.

#![allow(dead_code)]

use crate::transport::SessionError;
use deskoryn_core::trust::{CertFingerprint, TrustStore};
use deskoryn_core::DeviceId;
use std::sync::Arc;

/// A generated device identity: a self-signed certificate + private key (DER).
pub struct DeviceIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: CertFingerprint,
}

impl DeviceIdentity {
    /// Generate a fresh self-signed certificate whose subject encodes the
    /// [`DeviceId`]. Persist the DER under the state dir (see `config::Paths`).
    pub fn generate(device: DeviceId) -> Result<Self, SessionError> {
        let subject = format!("deskoryn:{device}");
        let cert = rcgen::generate_simple_self_signed(vec![subject])
            .map_err(|e| SessionError::Transport(format!("rcgen: {e}")))?;
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.signing_key.serialize_der();
        let fingerprint = CertFingerprint::of_der(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }
}

/// Endpoint that both listens for and dials peers.
pub struct QuicEndpoint {
    // endpoint: quinn::Endpoint,
    identity: Arc<DeviceIdentity>,
    trust: Arc<tokio::sync::Mutex<TrustStore>>,
}

impl QuicEndpoint {
    /// Bind a QUIC endpoint on `port` (0 = OS-assigned) configured for mutual
    /// TLS using `identity`, accepting peers verified against `trust`.
    pub async fn bind(
        _port: u16,
        _identity: Arc<DeviceIdentity>,
        _trust: Arc<tokio::sync::Mutex<TrustStore>>,
    ) -> Result<Self, SessionError> {
        // TODO(impl):
        //  - build a rustls ServerConfig + ClientConfig with our cert/key
        //  - install a custom ServerCertVerifier / ClientCertVerifier that hashes
        //    the presented end-entity cert and checks TrustStore::verify, OR
        //    accepts unknown certs while a PairingSession is active
        //  - wrap as quinn::ServerConfig / ClientConfig (QuicServerConfig)
        //  - quinn::Endpoint::server(...) and set_default_client_config(...)
        todo!("wire quinn + rustls mutual-TLS endpoint")
    }

    /// The local UDP port actually bound (to advertise over mDNS).
    pub fn local_port(&self) -> u16 {
        todo!()
    }

    /// Dial a known peer and return an established [`Session`](crate::Session).
    pub async fn connect(
        &self,
        _addr: std::net::SocketAddr,
        _expect: DeviceId,
    ) -> Result<Box<dyn crate::Session>, SessionError> {
        todo!("connect, verify pinned fingerprint, wrap in QuicSession")
    }

    /// Accept the next inbound session (post-verification).
    pub async fn accept(&self) -> Result<Box<dyn crate::Session>, SessionError> {
        todo!("accept connection, wrap in QuicSession")
    }
}

// A `QuicSession` would implement `crate::transport::Session` by lazily opening
// one bidi stream per Channel and routing `send_datagram`/`recv_datagram` to
// `quinn::Connection::{send_datagram, read_datagram}`.
