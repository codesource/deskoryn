//! A minimal jitter buffer: reorders by sequence number and conceals gaps.
//!
//! Depth (`target_frames`) trades latency for smoothness and is set from the
//! [`AudioProfile`](deskoryn_core::config::AudioProfile).

use std::collections::BTreeMap;

pub struct JitterBuffer {
    target_frames: usize,
    next_seq: u32,
    pending: BTreeMap<u32, Vec<u8>>,
    started: bool,
}

/// What the playback loop should do for one tick.
pub enum Pop {
    /// Play this packet.
    Packet(Vec<u8>),
    /// The expected packet is missing; run packet-loss concealment.
    Conceal,
    /// Not enough buffered yet; output silence and keep filling.
    Underrun,
}

impl JitterBuffer {
    pub fn new(target_frames: usize) -> Self {
        Self {
            target_frames: target_frames.max(1),
            next_seq: 0,
            pending: BTreeMap::new(),
            started: false,
        }
    }

    /// Recommended depth for a profile, in frames.
    pub fn depth_for(profile: deskoryn_core::config::AudioProfile) -> usize {
        use deskoryn_core::config::AudioProfile::*;
        match profile {
            LowLatency => 2,  // ~minimal; favors responsiveness
            HighQuality => 8, // smoother under jitter
        }
    }

    pub fn push(&mut self, seq: u32, packet: Vec<u8>) {
        // Drop packets older than what we've already played out.
        if self.started && seq < self.next_seq {
            return;
        }
        self.pending.insert(seq, packet);
    }

    /// Advance one frame. Call at the audio frame cadence.
    pub fn pop(&mut self) -> Pop {
        if !self.started {
            if self.pending.len() < self.target_frames {
                return Pop::Underrun;
            }
            // Prime: start at the lowest buffered sequence.
            self.next_seq = *self.pending.keys().next().unwrap();
            self.started = true;
        }
        match self.pending.remove(&self.next_seq) {
            Some(pkt) => {
                self.next_seq = self.next_seq.wrapping_add(1);
                Pop::Packet(pkt)
            }
            None => {
                // Gap: conceal and skip it so we don't stall forever.
                self.next_seq = self.next_seq.wrapping_add(1);
                Pop::Conceal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders_and_conceals() {
        let mut jb = JitterBuffer::new(2);
        jb.push(5, vec![5]);
        jb.push(7, vec![7]); // 6 is lost
        // Out-of-order arrival of the prime set:
        assert!(matches!(jb.pop(), Pop::Packet(p) if p == vec![5]));
        assert!(matches!(jb.pop(), Pop::Conceal)); // seq 6 missing
        assert!(matches!(jb.pop(), Pop::Packet(p) if p == vec![7]));
    }
}
