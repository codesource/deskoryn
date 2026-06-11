//! Aggregate per-file progress into something the tray UI can render: overall
//! percentage, bytes/sec, and an ETA.

use deskoryn_proto::StreamTag;

#[derive(Clone, Debug)]
pub struct ProgressTracker {
    pub tag: StreamTag,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub file_count: u32,
    pub files_done: u32,
    /// Throughput in bytes/sec from a simple moving estimate (filled by the
    /// daemon, which has the clock — this crate stays time-free for testability).
    pub bytes_per_sec: u64,
}

impl ProgressTracker {
    pub fn new(tag: StreamTag, total_bytes: u64, file_count: u32) -> Self {
        Self {
            tag,
            total_bytes,
            done_bytes: 0,
            file_count,
            files_done: 0,
            bytes_per_sec: 0,
        }
    }

    pub fn advance(&mut self, bytes: u64) {
        self.done_bytes = (self.done_bytes + bytes).min(self.total_bytes);
    }

    pub fn complete_file(&mut self) {
        self.files_done = (self.files_done + 1).min(self.file_count);
    }

    /// 0.0..=1.0
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return if self.files_done >= self.file_count { 1.0 } else { 0.0 };
        }
        self.done_bytes as f32 / self.total_bytes as f32
    }

    /// Estimated seconds remaining, or `None` until throughput is known.
    pub fn eta_secs(&self) -> Option<u64> {
        if self.bytes_per_sec == 0 {
            return None;
        }
        let remaining = self.total_bytes.saturating_sub(self.done_bytes);
        Some(remaining / self.bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_and_eta() {
        let mut p = ProgressTracker::new(1, 1000, 2);
        p.advance(250);
        assert!((p.fraction() - 0.25).abs() < 1e-6);
        assert_eq!(p.eta_secs(), None);
        p.bytes_per_sec = 250;
        assert_eq!(p.eta_secs(), Some(3)); // 750 remaining / 250
    }
}
