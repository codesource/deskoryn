//! LAN peer discovery.
//!
//! Production uses mDNS/DNS-SD (service type `_deskoryn._udp.local.`), the same
//! family of mechanisms KDE Connect and friends rely on. A device advertises its
//! [`DeviceId`], friendly name, QUIC port, and certificate fingerprint so a peer
//! can match an advertisement to a known trust record before connecting. Manual
//! `host:port` entry is always available for networks where multicast is blocked.

use async_trait::async_trait;
use deskoryn_core::trust::CertFingerprint;
use deskoryn_core::DeviceId;
use std::net::SocketAddr;

/// A discovered (or manually configured) peer endpoint.
#[derive(Clone, Debug)]
pub struct PeerHint {
    pub device: DeviceId,
    pub name: String,
    pub addr: SocketAddr,
    /// Advertised certificate fingerprint, if any. Lets us reject an imposter
    /// before opening a connection (it must still match the pinned value).
    pub fingerprint: Option<CertFingerprint>,
}

/// The Deskoryn DNS-SD service type.
pub const SERVICE_TYPE: &str = "_deskoryn._udp.local.";

#[async_trait]
pub trait Discovery: Send + Sync {
    /// Begin advertising this device on the LAN.
    async fn advertise(&self, device: DeviceId, name: &str, port: u16, fp: CertFingerprint)
        -> std::io::Result<()>;

    /// Receive the next discovered peer. Implementations dedupe/refresh TTLs and
    /// only surface changes.
    async fn next_peer(&self) -> Option<PeerHint>;

    async fn shutdown(&self);
}

/// A no-op discovery used when `mdns` is disabled or in `--dry-run`. Peers must
/// then be provided via `network.static_peers` in config.
pub struct NullDiscovery;

#[async_trait]
impl Discovery for NullDiscovery {
    async fn advertise(&self, _d: DeviceId, _n: &str, _p: u16, _f: CertFingerprint) -> std::io::Result<()> {
        Ok(())
    }
    async fn next_peer(&self) -> Option<PeerHint> {
        std::future::pending().await
    }
    async fn shutdown(&self) {}
}

// The real mDNS implementation lives behind the `mdns` feature.
#[cfg(feature = "mdns")]
pub mod mdns {
    //! mDNS-SD backend built on the `mdns-sd` crate.
    //!
    //! TODO(impl): register a `ServiceInfo` for [`SERVICE_TYPE`] carrying TXT
    //! records `id=<hex>`, `name=<utf8>`, `fp=<hex>`; browse the same type and
    //! map each resolved service into a [`PeerHint`]. Refresh on TTL, and prefer
    //! `trusted.json` `last_address` as a fast-path before multicast resolves.
}
