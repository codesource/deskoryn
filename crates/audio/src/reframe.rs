//! Re-framing buffer: turn arbitrary PCM blocks into fixed-size frames.
//!
//! OS capture backends (cpal) deliver interleaved PCM in whatever block size the
//! device chooses, but Opus only encodes fixed frame durations (2.5/5/10/20/40/60
//! ms). The source pump pushes captured blocks through a [`Reframer`] and encodes
//! each emitted fixed frame. Pure logic — unit-tested, no I/O.

use crate::{AudioError, Capture};
use async_trait::async_trait;
use deskoryn_core::config::AudioProfile;

/// Opus frame duration per profile: short for low-latency, longer (better
/// compression / fewer packets) for quality. Both are valid Opus frame sizes.
pub fn frame_ms(profile: AudioProfile) -> u32 {
    match profile {
        AudioProfile::LowLatency => 10,
        AudioProfile::HighQuality => 20,
    }
}

/// Interleaved-sample count of one frame at `rate`/`channels` for `profile`.
pub fn frame_len(rate: u32, channels: u8, profile: AudioProfile) -> usize {
    let per_channel = (rate as usize / 1000) * frame_ms(profile) as usize;
    (per_channel * channels.max(1) as usize).max(1)
}

/// Accumulates interleaved PCM and yields fixed-length frames.
pub struct Reframer {
    frame_len: usize,
    buf: Vec<f32>,
}

impl Reframer {
    /// `frame_len` is the number of **interleaved** samples per emitted frame
    /// (samples-per-channel × channels).
    pub fn new(frame_len: usize) -> Self {
        Self {
            frame_len: frame_len.max(1),
            buf: Vec::new(),
        }
    }

    pub fn for_profile(rate: u32, channels: u8, profile: AudioProfile) -> Self {
        Self::new(frame_len(rate, channels, profile))
    }

    /// The emitted frame size, in interleaved samples.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn push(&mut self, block: &[f32]) {
        self.buf.extend_from_slice(block);
    }

    /// Pop one full frame if enough samples have accumulated.
    pub fn pop(&mut self) -> Option<Vec<f32>> {
        if self.buf.len() >= self.frame_len {
            Some(self.buf.drain(..self.frame_len).collect())
        } else {
            None
        }
    }
}

/// Wraps a [`Capture`] so `next_frame` yields only fixed-size frames suitable
/// for an Opus encoder, regardless of the device's native block size. A trailing
/// partial frame (less than one full frame at end-of-stream) is dropped.
pub struct ReframingCapture {
    inner: Box<dyn Capture>,
    reframer: Reframer,
}

impl ReframingCapture {
    pub fn new(inner: Box<dyn Capture>, profile: AudioProfile) -> Self {
        let reframer = Reframer::for_profile(inner.sample_rate(), inner.channels(), profile);
        Self { inner, reframer }
    }
}

#[async_trait]
impl Capture for ReframingCapture {
    async fn next_frame(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
        loop {
            if let Some(frame) = self.reframer.pop() {
                return Ok(Some(frame));
            }
            match self.inner.next_frame().await? {
                Some(block) => self.reframer.push(&block),
                None => return Ok(None),
            }
        }
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn channels(&self) -> u8 {
        self.inner.channels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_len_is_20ms_stereo_at_48k() {
        // 20 ms @ 48 kHz = 960 samples/channel; stereo ⇒ 1920 interleaved.
        assert_eq!(frame_len(48_000, 2, AudioProfile::HighQuality), 1920);
        // 10 ms low-latency mono ⇒ 480.
        assert_eq!(frame_len(48_000, 1, AudioProfile::LowLatency), 480);
    }

    #[test]
    fn reframes_irregular_blocks_into_fixed_frames() {
        let mut rf = Reframer::new(4);
        rf.push(&[1.0, 2.0, 3.0]); // 3 < 4
        assert!(rf.pop().is_none());
        rf.push(&[4.0, 5.0, 6.0, 7.0, 8.0]); // total 8 ⇒ two frames
        assert_eq!(rf.pop().unwrap(), vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rf.pop().unwrap(), vec![5.0, 6.0, 7.0, 8.0]);
        assert!(rf.pop().is_none());
    }

    #[test]
    fn keeps_remainder_between_pushes() {
        let mut rf = Reframer::new(3);
        rf.push(&[1.0, 2.0, 3.0, 4.0]); // one frame + remainder [4.0]
        assert_eq!(rf.pop().unwrap(), vec![1.0, 2.0, 3.0]);
        assert!(rf.pop().is_none());
        rf.push(&[5.0, 6.0]); // remainder [4,5,6] ⇒ one frame
        assert_eq!(rf.pop().unwrap(), vec![4.0, 5.0, 6.0]);
    }
}
