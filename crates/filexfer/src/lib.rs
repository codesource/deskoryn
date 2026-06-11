//! # deskoryn-filexfer
//!
//! Background file and folder transfer that backs three user-visible features:
//! drag-and-drop between machines, file clipboard paste, and (optionally) shared
//! folder sync. Architecturally close to Syncthing's block model, scaled down for
//! a trusted two-peer LAN.
//!
//! ## Wire shape
//!
//! Control messages ([`FileXfer`](deskoryn_proto::FileXfer)) travel on the
//! reliable FileXfer channel; the *bytes* travel on a dedicated per-transfer
//! stream as a sequence of chunks framed `[u32 file_index][u64 offset][u32 len][bytes]`.
//! Each file is hashed (BLAKE3) so transfers can **resume** from an offset and
//! duplicate content can be skipped.
//!
//! ## Progress & conflicts
//!
//! [`ProgressTracker`] aggregates per-file progress into an overall percentage +
//! throughput estimate for the tray UI. [`resolve_conflict`] applies the user's
//! [`ConflictPolicy`](deskoryn_core::config::ConflictPolicy) when a destination
//! file already exists.

pub mod manifest;
pub mod progress;
pub mod sink;

use deskoryn_core::config::ConflictPolicy;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("hash mismatch on {0} (resume invalid or corruption)")]
    HashMismatch(String),
    #[error("transfer cancelled: {0}")]
    Cancelled(String),
    #[error("path escapes destination root: {0}")]
    UnsafePath(String),
}

/// Decide the final destination path for an incoming file whose intended path is
/// `dest` under transfer root `root`, applying `policy` when it already exists.
///
/// Returns `Ok(None)` when the policy says to skip the file.
pub fn resolve_conflict(
    root: &Path,
    dest: &Path,
    policy: ConflictPolicy,
) -> Result<Option<PathBuf>, TransferError> {
    // Guard against path traversal in a peer-supplied relative path.
    if !dest.starts_with(root) {
        return Err(TransferError::UnsafePath(dest.display().to_string()));
    }
    if !dest.exists() {
        return Ok(Some(dest.to_path_buf()));
    }
    match policy {
        ConflictPolicy::Overwrite => Ok(Some(dest.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        // `Ask` is resolved upstream by the UI; if it reaches here, fall back to
        // the safe option (rename) rather than clobbering data.
        ConflictPolicy::Rename | ConflictPolicy::Ask => Ok(Some(unique_name(dest))),
    }
}

/// Turn `/x/report.pdf` into `/x/report (2).pdf`, incrementing until free.
fn unique_name(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = dest.extension().and_then(|s| s.to_str());
    for n in 2..10_000 {
        let name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Absurd fallback; effectively unreachable on a real filesystem.
    parent.join(format!("{stem}.dup"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        let root = Path::new("/tmp/deskoryn-dl");
        let evil = Path::new("/etc/passwd");
        assert!(matches!(
            resolve_conflict(root, evil, ConflictPolicy::Overwrite),
            Err(TransferError::UnsafePath(_))
        ));
    }
}
