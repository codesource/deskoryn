//! Stable identifiers for devices and monitors.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A stable, per-installation device identifier.
///
/// Generated once on first run and persisted in the config directory. It is
/// independent of IP address or hostname so a device keeps its identity across
/// network changes. The id is also bound into the device's TLS certificate (see
/// `deskoryn-net`), and the [`trust`](crate::trust) store pins the certificate
/// fingerprint to this id (trust-on-first-use).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId([u8; 16]);

impl DeviceId {
    /// Generate a fresh random id. Call once per installation and persist it.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
        Self(bytes)
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Short human-facing form used in pairing dialogs and logs (8 hex chars).
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

impl std::str::FromStr for DeviceId {
    type Err = hex::FromHexError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 16];
        hex::decode_to_slice(s, &mut bytes)?;
        Ok(Self(bytes))
    }
}

/// Identifies a single monitor within the [virtual desktop](crate::layout).
///
/// Scoped by owning device so the two machines never collide. `index` is the
/// position reported by the platform's display enumeration on the owning device.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct MonitorId {
    pub device: DeviceId,
    pub index: u16,
}

impl MonitorId {
    pub const fn new(device: DeviceId, index: u16) -> Self {
        Self { device, index }
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.device.short(), self.index)
    }
}
