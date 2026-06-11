//! Device pairing and the short-authentication-string (SAS) verification flow.
//!
//! Goal: defeat a man-in-the-middle on first contact without any pre-shared
//! secret or central authority. The approach mirrors the "compare a number on
//! both screens" UX of Bluetooth SSP / Signal safety numbers:
//!
//! 1. Both peers complete the TLS 1.3 handshake (each presents a self-signed
//!    certificate bound to its [`DeviceId`]).
//! 2. Each side computes a [`ShortAuthString`] deterministically from *both*
//!    certificate fingerprints (order-independent) plus the TLS exporter secret.
//! 3. The user confirms the strings match (6 digits, also renderable as a QR /
//!    emoji set). A MITM would present different certificates → different SAS.
//! 4. On confirmation each side pins the other's fingerprint into the
//!    [`TrustStore`](deskoryn_core::trust::TrustStore).
//!
//! After pairing, reconnection is automatic and silent: the pinned fingerprint
//! is checked during TLS and no user interaction occurs.

use deskoryn_core::trust::{CertFingerprint, TrustedDevice};
use deskoryn_core::DeviceId;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("the user rejected the pairing")]
    Rejected,
    #[error("short auth strings did not match")]
    SasMismatch,
    #[error("pairing timed out")]
    Timeout,
}

/// A 6-digit code shown on both machines during pairing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShortAuthString(pub u32);

impl ShortAuthString {
    /// Derive the SAS from both fingerprints (commutative) and the unique TLS
    /// channel binding. Identical inputs on both peers ⇒ identical output; a
    /// MITM cannot match it because each leg has a different channel binding.
    pub fn derive(local: &CertFingerprint, remote: &CertFingerprint, tls_exporter: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        // Sort the two fingerprints so both peers hash in the same order.
        let (a, b) = if local.0 <= remote.0 {
            (local, remote)
        } else {
            (remote, local)
        };
        h.update(b"deskoryn-sas-v1");
        h.update(&a.0);
        h.update(&b.0);
        h.update(tls_exporter);
        let bytes = h.finalize();
        let n = u32::from_le_bytes([bytes.as_bytes()[0], bytes.as_bytes()[1], bytes.as_bytes()[2], bytes.as_bytes()[3]]);
        ShortAuthString(n % 1_000_000)
    }

    /// Zero-padded display form, e.g. `042 137`.
    pub fn display(&self) -> String {
        let s = format!("{:06}", self.0);
        format!("{} {}", &s[..3], &s[3..])
    }
}

/// State for an in-progress pairing, independent of UI and transport.
pub struct PairingSession {
    pub local: DeviceId,
    pub local_fp: CertFingerprint,
    pub remote: DeviceId,
    pub remote_fp: CertFingerprint,
    pub remote_name: String,
    pub sas: ShortAuthString,
}

impl PairingSession {
    pub fn new(
        local: DeviceId,
        local_fp: CertFingerprint,
        remote: DeviceId,
        remote_fp: CertFingerprint,
        remote_name: String,
        tls_exporter: &[u8],
    ) -> Self {
        let sas = ShortAuthString::derive(&local_fp, &remote_fp, tls_exporter);
        Self {
            local,
            local_fp,
            remote,
            remote_fp,
            remote_name,
            sas,
        }
    }

    /// Call once the user has confirmed the SAS matches on both screens. Returns
    /// the trust record to persist.
    pub fn confirm(&self, now_unix: u64, last_address: Option<String>) -> TrustedDevice {
        TrustedDevice {
            id: self.remote,
            name: self.remote_name.clone(),
            fingerprint: self.remote_fp,
            paired_at: now_unix,
            last_address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_peers_derive_same_sas() {
        let a = CertFingerprint([7; 32]);
        let b = CertFingerprint([9; 32]);
        let exporter = b"shared-tls-channel-binding";
        let sas_a = ShortAuthString::derive(&a, &b, exporter);
        let sas_b = ShortAuthString::derive(&b, &a, exporter); // swapped order
        assert_eq!(sas_a, sas_b);
        assert_eq!(sas_a.display().len(), 7); // "ddd ddd"
    }

    #[test]
    fn mitm_with_different_binding_fails_to_match() {
        let a = CertFingerprint([7; 32]);
        let b = CertFingerprint([9; 32]);
        let real = ShortAuthString::derive(&a, &b, b"binding-1");
        let mitm = ShortAuthString::derive(&a, &b, b"binding-2");
        assert_ne!(real, mitm);
    }
}
