//! QUIC + TLS 1.3 transport backend (feature = "quic").
//!
//! Channel → transport mapping (see `docs/PROTOCOL.md`):
//!
//! * Each reliable [`Channel`](deskoryn_proto::Channel) is one bidirectional
//!   QUIC stream, established up-front and tagged with a one-byte channel id so
//!   both peers agree on which stream is which. Messages are length-prefixed
//!   ([`deskoryn_proto::framing`]).
//! * The audio channel uses **QUIC datagrams** (unreliable, no head-of-line
//!   blocking) so a lost Opus packet never stalls input or clipboard traffic.
//! * Mutual authentication: both endpoints present self-signed certificates
//!   bound to their [`DeviceId`]. The custom [`PinVerifier`] performs the TLS
//!   signature check (proving the peer holds the cert's private key) but skips CA
//!   chain/name validation; identity is then enforced by matching the peer's
//!   certificate fingerprint against the pin in the trust store (trust-on-first-
//!   use). The peer's `DeviceId` is recovered by reverse-looking-up that
//!   fingerprint in the trust store.

#![allow(dead_code)]

use crate::transport::{Session, SessionError, Sink, Source};
use async_trait::async_trait;
use deskoryn_core::trust::{CertFingerprint, TrustStore};
use deskoryn_core::DeviceId;
use deskoryn_proto::framing::MAX_FRAME_LEN;
use deskoryn_proto::Channel;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// ALPN identifier negotiated on every Deskoryn QUIC connection.
const ALPN: &[u8] = b"deskoryn/1";

fn channel_id(c: Channel) -> u8 {
    match c {
        Channel::Control => 0,
        Channel::Input => 1,
        Channel::Clipboard => 2,
        Channel::FileXfer => 3,
        Channel::Audio => 4,
    }
}

const CHANNEL_COUNT: u8 = 5;

fn te<E: std::fmt::Display>(e: E) -> SessionError {
    SessionError::Transport(e.to_string())
}

// ---------------------------------------------------------------------------
// Device identity (self-signed cert + key)
// ---------------------------------------------------------------------------

/// A generated device identity: a self-signed certificate + private key (DER).
pub struct DeviceIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: CertFingerprint,
}

impl DeviceIdentity {
    /// Generate a fresh self-signed certificate whose subject encodes the
    /// [`DeviceId`].
    pub fn generate(device: DeviceId) -> Result<Self, SessionError> {
        let subject = format!("deskoryn-{device}");
        let cert = rcgen::generate_simple_self_signed(vec![subject]).map_err(te)?;
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();
        let fingerprint = CertFingerprint::of_der(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }

    /// Load a persisted identity from `cert_path`/`key_path`, generating and
    /// saving a fresh one if either file is missing.
    pub fn load_or_generate(
        device: DeviceId,
        cert_path: &std::path::Path,
        key_path: &std::path::Path,
    ) -> Result<Self, SessionError> {
        if cert_path.exists() && key_path.exists() {
            let cert_der = std::fs::read(cert_path).map_err(te)?;
            let key_der = std::fs::read(key_path).map_err(te)?;
            let fingerprint = CertFingerprint::of_der(&cert_der);
            return Ok(Self {
                cert_der,
                key_der,
                fingerprint,
            });
        }
        let id = Self::generate(device)?;
        if let Some(parent) = cert_path.parent() {
            std::fs::create_dir_all(parent).map_err(te)?;
        }
        std::fs::write(cert_path, &id.cert_der).map_err(te)?;
        std::fs::write(key_path, &id.key_der).map_err(te)?;
        // Best-effort tighten the private key permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(id)
    }

    fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.cert_der.clone())]
    }

    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }
}

// ---------------------------------------------------------------------------
// Certificate verifier: accept any cert (signature-checked), pin afterwards
// ---------------------------------------------------------------------------

/// Accepts any presented certificate after verifying its handshake signature,
/// deferring *identity* enforcement to the post-handshake fingerprint pin. This
/// is the correct shape for TOFU: TLS proves the peer owns the key for cert `X`,
/// and the application checks `fingerprint(X)` against the trust store.
#[derive(Debug)]
struct PinVerifier {
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    fn new(provider: &rustls::crypto::CryptoProvider) -> Arc<Self> {
        Arc::new(Self {
            algs: provider.signature_verification_algorithms,
        })
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl rustls::server::danger::ClientCertVerifier for PinVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn build_server_config(identity: &DeviceIdentity) -> Result<quinn::ServerConfig, SessionError> {
    let provider = crypto_provider();
    let verifier = PinVerifier::new(&provider);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(te)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.cert_chain(), identity.private_key())
        .map_err(te)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(te)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(qsc)))
}

fn build_client_config(identity: &DeviceIdentity) -> Result<quinn::ClientConfig, SessionError> {
    let provider = crypto_provider();
    let verifier = PinVerifier::new(&provider);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(te)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(identity.cert_chain(), identity.private_key())
        .map_err(te)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(te)?;
    Ok(quinn::ClientConfig::new(Arc::new(qcc)))
}

/// Extract the peer's end-entity certificate fingerprint from an established
/// connection (after the handshake has cryptographically verified key possession).
fn peer_fingerprint(conn: &quinn::Connection) -> Result<CertFingerprint, SessionError> {
    let identity = conn
        .peer_identity()
        .ok_or_else(|| SessionError::Transport("peer presented no certificate".into()))?;
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| SessionError::Transport("unexpected peer identity type".into()))?;
    let first = certs
        .first()
        .ok_or_else(|| SessionError::Transport("empty peer certificate chain".into()))?;
    Ok(CertFingerprint::of_der(first.as_ref()))
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

type StreamSlot = (Option<quinn::SendStream>, Option<quinn::RecvStream>);

pub struct QuicSession {
    peer: DeviceId,
    fingerprint: CertFingerprint,
    conn: quinn::Connection,
    streams: Mutex<HashMap<u8, StreamSlot>>,
}

impl QuicSession {
    /// Establish the per-channel streams over an already-connected QUIC
    /// connection. The dialing side opens streams and tags them; the accepting
    /// side reads the tag and routes accordingly.
    async fn establish(
        conn: quinn::Connection,
        is_client: bool,
        peer: DeviceId,
        fingerprint: CertFingerprint,
    ) -> Result<Self, SessionError> {
        let mut streams: HashMap<u8, StreamSlot> = HashMap::new();
        if is_client {
            for id in 0..CHANNEL_COUNT {
                let (mut send, recv) = conn.open_bi().await.map_err(te)?;
                send.write_all(&[id]).await.map_err(te)?;
                streams.insert(id, (Some(send), Some(recv)));
            }
        } else {
            for _ in 0..CHANNEL_COUNT {
                let (send, mut recv) = conn.accept_bi().await.map_err(te)?;
                let mut tag = [0u8; 1];
                recv.read_exact(&mut tag).await.map_err(te)?;
                streams.insert(tag[0], (Some(send), Some(recv)));
            }
        }
        Ok(Self {
            peer,
            fingerprint,
            conn,
            streams: Mutex::new(streams),
        })
    }

    pub fn fingerprint(&self) -> CertFingerprint {
        self.fingerprint
    }

    /// Derive a shared secret bound to *this* TLS connection (RFC 5705 exporter).
    /// Both peers obtain identical bytes; a man-in-the-middle terminating two
    /// separate TLS sessions cannot. Used as the channel binding for pairing SAS.
    pub fn channel_binding(&self) -> Result<[u8; 32], SessionError> {
        let mut out = [0u8; 32];
        self.conn
            .export_keying_material(&mut out, b"deskoryn-pairing-sas-v1", b"")
            .map_err(|_| SessionError::Transport("TLS exporter unavailable".into()))?;
        Ok(out)
    }
}

#[async_trait]
impl Session for QuicSession {
    fn peer(&self) -> DeviceId {
        self.peer
    }

    async fn channel(
        &self,
        channel: Channel,
    ) -> Result<(Box<dyn Sink>, Box<dyn Source>), SessionError> {
        let id = channel_id(channel);
        let mut map = self.streams.lock().await;
        let slot = map.get_mut(&id).ok_or(SessionError::NoChannel(channel))?;
        let send = slot.0.take().ok_or(SessionError::NoChannel(channel))?;
        let recv = slot.1.take().ok_or(SessionError::NoChannel(channel))?;
        Ok((Box::new(QuicSink(send)), Box::new(QuicSource(recv))))
    }

    async fn send_datagram(&self, bytes: &[u8]) -> Result<(), SessionError> {
        self.conn
            .send_datagram(bytes::Bytes::copy_from_slice(bytes))
            .map_err(te)
    }

    async fn recv_datagram(&self) -> Result<Option<Vec<u8>>, SessionError> {
        match self.conn.read_datagram().await {
            Ok(b) => Ok(Some(b.to_vec())),
            Err(quinn::ConnectionError::LocallyClosed)
            | Err(quinn::ConnectionError::ApplicationClosed(_)) => Ok(None),
            Err(e) => Err(te(e)),
        }
    }

    async fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }
}

struct QuicSink(quinn::SendStream);

#[async_trait]
impl Sink for QuicSink {
    async fn send_bytes(&mut self, frame: &[u8]) -> Result<(), SessionError> {
        self.0.write_all(frame).await.map_err(te)
    }
    async fn flush(&mut self) -> Result<(), SessionError> {
        Ok(())
    }
}

struct QuicSource(quinn::RecvStream);

#[async_trait]
impl Source for QuicSource {
    async fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
        // Read one length-prefixed frame; return prefix+body so the caller's
        // `decode_one` sees a complete frame (matching the loopback contract).
        let mut len_buf = [0u8; 4];
        match self.0.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(te(e)),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_LEN {
            return Err(SessionError::Transport(format!("frame too large: {len}")));
        }
        let mut out = Vec::with_capacity(4 + len);
        out.extend_from_slice(&len_buf);
        let start = out.len();
        out.resize(start + len, 0);
        self.0.read_exact(&mut out[start..]).await.map_err(te)?;
        Ok(Some(out))
    }
}

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// A QUIC endpoint that both listens for and dials peers, sharing one identity
/// and trust store.
pub struct QuicEndpoint {
    endpoint: quinn::Endpoint,
    trust: Arc<Mutex<TrustStore>>,
}

impl QuicEndpoint {
    /// Bind on `port` (0 = OS-assigned) for mutual-TLS QUIC, authenticating
    /// peers against `trust`.
    pub async fn bind(
        port: u16,
        identity: Arc<DeviceIdentity>,
        trust: Arc<Mutex<TrustStore>>,
    ) -> Result<Self, SessionError> {
        let server_config = build_server_config(&identity)?;
        let client_config = build_client_config(&identity)?;
        let addr: SocketAddr = format!("0.0.0.0:{port}").parse().map_err(te)?;
        let mut endpoint = quinn::Endpoint::server(server_config, addr).map_err(te)?;
        endpoint.set_default_client_config(client_config);
        Ok(Self { endpoint, trust })
    }

    /// The local UDP port actually bound (to advertise over mDNS).
    pub fn local_port(&self) -> u16 {
        self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Dial a known peer and verify its certificate matches the pin for `expect`.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        expect: DeviceId,
    ) -> Result<Box<dyn Session>, SessionError> {
        let conn = self
            .endpoint
            .connect(addr, "deskoryn")
            .map_err(te)?
            .await
            .map_err(te)?;
        let fp = peer_fingerprint(&conn)?;
        {
            let trust = self.trust.lock().await;
            if !trust.verify(expect, &fp) {
                conn.close(1u32.into(), b"unrecognized certificate");
                return Err(SessionError::Transport(format!(
                    "peer {} presented an unpinned certificate",
                    expect.short()
                )));
            }
        }
        let session = QuicSession::establish(conn, true, expect, fp).await?;
        Ok(Box::new(session))
    }

    /// Dial a peer whose `DeviceId` we don't know in advance (e.g. a static
    /// address or a remembered `last_address`), identifying it by reverse-
    /// looking-up its certificate fingerprint in the trust store. Rejected if
    /// the peer is not already trusted.
    pub async fn connect_any(&self, addr: SocketAddr) -> Result<Box<dyn Session>, SessionError> {
        let conn = self
            .endpoint
            .connect(addr, "deskoryn")
            .map_err(te)?
            .await
            .map_err(te)?;
        let fp = peer_fingerprint(&conn)?;
        let peer = {
            let trust = self.trust.lock().await;
            trust.devices.iter().find(|d| d.fingerprint == fp).map(|d| d.id)
        };
        let Some(peer) = peer else {
            conn.close(1u32.into(), b"unpaired");
            return Err(SessionError::Transport("peer at address is not paired".into()));
        };
        let session = QuicSession::establish(conn, true, peer, fp).await?;
        Ok(Box::new(session))
    }

    /// Dial a peer for **pairing**: establish the connection without consulting
    /// the trust store, returning the concrete session so the caller can read its
    /// [`fingerprint`](QuicSession::fingerprint) and
    /// [`channel_binding`](QuicSession::channel_binding) to compute the SAS. The
    /// returned session's `peer()` is a placeholder until pairing learns the id.
    pub async fn connect_unverified(&self, addr: SocketAddr) -> Result<QuicSession, SessionError> {
        let conn = self
            .endpoint
            .connect(addr, "deskoryn")
            .map_err(te)?
            .await
            .map_err(te)?;
        let fp = peer_fingerprint(&conn)?;
        QuicSession::establish(conn, true, DeviceId::from_bytes([0u8; 16]), fp).await
    }

    /// Accept an inbound connection for **pairing**, without a trust check.
    pub async fn accept_unverified(&self) -> Result<QuicSession, SessionError> {
        let incoming = self.endpoint.accept().await.ok_or(SessionError::Closed)?;
        let conn = incoming.await.map_err(te)?;
        let fp = peer_fingerprint(&conn)?;
        QuicSession::establish(conn, false, DeviceId::from_bytes([0u8; 16]), fp).await
    }

    /// Accept the next inbound connection, identifying the peer by reverse-
    /// looking-up its certificate fingerprint in the trust store. Unknown
    /// (unpaired) peers are rejected here; pairing uses [`accept_unverified`].
    pub async fn accept(&self) -> Result<Box<dyn Session>, SessionError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(SessionError::Closed)?;
        let conn = incoming.await.map_err(te)?;
        let fp = peer_fingerprint(&conn)?;
        let peer = {
            let trust = self.trust.lock().await;
            trust.devices.iter().find(|d| d.fingerprint == fp).map(|d| d.id)
        };
        let Some(peer) = peer else {
            conn.close(1u32.into(), b"unpaired");
            return Err(SessionError::Transport("rejected unpaired peer".into()));
        };
        let session = QuicSession::establish(conn, false, peer, fp).await?;
        Ok(Box::new(session))
    }
}
