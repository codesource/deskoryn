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
    //! Advertises a `ServiceInfo` for [`SERVICE_TYPE`] carrying TXT records
    //! `id=<hex>`, `name=<utf8>`, `fp=<hex>`, and browses the same type, mapping
    //! each resolved service into a [`PeerHint`] (skipping our own id).

    use super::{Discovery, PeerHint, SERVICE_TYPE};
    use async_trait::async_trait;
    use deskoryn_core::trust::CertFingerprint;
    use deskoryn_core::DeviceId;
    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::{mpsc, Mutex};

    pub struct MdnsDiscovery {
        daemon: ServiceDaemon,
        rx: Mutex<mpsc::UnboundedReceiver<PeerHint>>,
        tx: mpsc::UnboundedSender<PeerHint>,
        /// Registered service fullname, kept so we can unregister on shutdown.
        registered: StdMutex<Option<String>>,
    }

    fn io(e: impl std::fmt::Display) -> std::io::Error {
        std::io::Error::other(e.to_string())
    }

    impl MdnsDiscovery {
        pub fn new() -> std::io::Result<Self> {
            let daemon = ServiceDaemon::new().map_err(io)?;
            let (tx, rx) = mpsc::unbounded_channel();
            Ok(Self {
                daemon,
                rx: Mutex::new(rx),
                tx,
                registered: StdMutex::new(None),
            })
        }

        fn spawn_browser(&self, local: DeviceId) -> std::io::Result<()> {
            let receiver = self.daemon.browse(SERVICE_TYPE).map_err(io)?;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                while let Ok(event) = receiver.recv_async().await {
                    if let ServiceEvent::ServiceResolved(info) = event {
                        if let Some(hint) = to_hint(&info) {
                            if hint.device != local {
                                let _ = tx.send(hint);
                            }
                        }
                    }
                }
            });
            Ok(())
        }
    }

    fn to_hint(info: &ServiceInfo) -> Option<PeerHint> {
        let device = info
            .get_property_val_str("id")
            .and_then(|s| DeviceId::from_str(s).ok())?;
        let name = info
            .get_property_val_str("name")
            .unwrap_or("deskoryn")
            .to_string();
        let fingerprint = info.get_property_val_str("fp").and_then(parse_fp);
        let ip = info.get_addresses().iter().next().copied()?;
        Some(PeerHint {
            device,
            name,
            addr: SocketAddr::new(ip, info.get_port()),
            fingerprint,
        })
    }

    fn parse_fp(s: &str) -> Option<CertFingerprint> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(s, &mut bytes).ok()?;
        Some(CertFingerprint(bytes))
    }

    #[async_trait]
    impl Discovery for MdnsDiscovery {
        async fn advertise(
            &self,
            device: DeviceId,
            name: &str,
            port: u16,
            fp: CertFingerprint,
        ) -> std::io::Result<()> {
            let instance = device.short();
            let host = format!("{}.local.", device.short());
            let id_s = device.to_string();
            let fp_s = hex::encode(fp.0);
            let props: [(&str, &str); 3] = [("id", &id_s), ("name", name), ("fp", &fp_s)];

            // Empty address + `enable_addr_auto` lets mdns-sd advertise the host's
            // current interface addresses (and update them as they change).
            let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, &props[..])
                .map_err(io)?
                .enable_addr_auto();
            let fullname = info.get_fullname().to_string();
            self.daemon.register(info).map_err(io)?;
            *self.registered.lock().unwrap() = Some(fullname);

            self.spawn_browser(device)?;
            Ok(())
        }

        async fn next_peer(&self) -> Option<PeerHint> {
            self.rx.lock().await.recv().await
        }

        async fn shutdown(&self) {
            if let Some(full) = self.registered.lock().unwrap().clone() {
                let _ = self.daemon.unregister(&full);
            }
            let _ = self.daemon.shutdown();
        }
    }
}
