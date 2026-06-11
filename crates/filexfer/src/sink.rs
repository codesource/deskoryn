//! Writing received chunks to disk with hashing and resume support.

use crate::TransferError;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// A file currently being received. Verifies content as it lands and supports
/// resuming from a byte offset (the receiver tells the sender where to start via
/// [`FileResume`](deskoryn_proto::FileResume)).
pub struct FileSink {
    file: std::fs::File,
    hasher: blake3::Hasher,
    pub written: u64,
}

impl FileSink {
    /// Open (creating parent dirs) for writing, seeking to `resume_from`.
    pub fn create(path: &Path, resume_from: u64) -> Result<Self, TransferError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(path)?;
        if resume_from > 0 {
            file.seek(SeekFrom::Start(resume_from))?;
        }
        // NOTE: to validate a resume we would re-hash the existing prefix here.
        Ok(Self {
            file,
            hasher: blake3::Hasher::new(),
            written: resume_from,
        })
    }

    pub fn write_chunk(&mut self, offset: u64, bytes: &[u8]) -> Result<(), TransferError> {
        // Chunks are expected in order on a per-file stream; offset is asserted
        // so out-of-order delivery surfaces loudly rather than corrupting data.
        if offset != self.written {
            return Err(TransferError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected offset {}, got {offset}", self.written),
            )));
        }
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Finalize: flush and verify the BLAKE3 hash against the manifest entry.
    pub fn finish(mut self, expected: Option<[u8; 32]>, name: &str) -> Result<(), TransferError> {
        self.file.flush()?;
        if let Some(exp) = expected {
            let got = *self.hasher.finalize().as_bytes();
            if got != exp {
                return Err(TransferError::HashMismatch(name.to_string()));
            }
        }
        Ok(())
    }
}
