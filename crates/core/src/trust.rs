//! The trust store: remembered, paired devices.
//!
//! Deskoryn uses **trust-on-first-use** with explicit out-of-band verification.
//! During pairing the user confirms a short code derived from both certificate
//! fingerprints (see `docs/SECURITY.md`). Once confirmed, the peer's id and
//! certificate fingerprint are pinned here; later connections are accepted only
//! if the presented certificate matches the pin.

use crate::ids::DeviceId;
use serde::{Deserialize, Serialize};

/// BLAKE3 hash of a peer's DER-encoded certificate (or SPKI). 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertFingerprint(pub [u8; 32]);

impl CertFingerprint {
    pub fn of_der(der: &[u8]) -> Self {
        Self(*blake3::hash(der).as_bytes())
    }

    /// Short, human-comparable form used in the pairing dialog: groups of the
    /// first bytes, e.g. `4F2A 9C10 ...`.
    pub fn short(&self) -> String {
        let b = &self.0;
        format!(
            "{:02X}{:02X} {:02X}{:02X} {:02X}{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl std::fmt::Debug for CertFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CertFingerprint({})", hex::encode(&self.0[..6]))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: DeviceId,
    pub name: String,
    pub fingerprint: CertFingerprint,
    /// Unix seconds at pairing time (recorded by the daemon).
    pub paired_at: u64,
    /// Last successfully connected address, used as a discovery hint.
    pub last_address: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    pub devices: Vec<TrustedDevice>,
}

impl TrustStore {
    pub fn get(&self, id: DeviceId) -> Option<&TrustedDevice> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// Returns true if `id` is paired *and* presents the pinned certificate.
    pub fn verify(&self, id: DeviceId, presented: &CertFingerprint) -> bool {
        self.get(id).is_some_and(|d| &d.fingerprint == presented)
    }

    /// Insert or replace the trust record for a device.
    pub fn upsert(&mut self, device: TrustedDevice) {
        match self.devices.iter_mut().find(|d| d.id == device.id) {
            Some(slot) => *slot = device,
            None => self.devices.push(device),
        }
    }

    pub fn forget(&mut self, id: DeviceId) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.id != id);
        self.devices.len() != before
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, bytes)
    }
}
