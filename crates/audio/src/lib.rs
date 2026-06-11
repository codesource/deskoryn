//! # deskoryn-audio
//!
//! Streams audio from one machine's output to the other's speakers (and
//! optionally the reverse), so e.g. the Windows machine's sound plays through the
//! Linux machine's headphones. Conceptually like Scream/VBAN but riding the same
//! secure session as everything else, encoded with Opus.
//!
//! ## Pipeline
//!
//! ```text
//!  source machine                                   destination machine
//!  ┌────────────┐   PCM    ┌─────────┐  Opus   ┌──────────┐  PCM   ┌──────────┐
//!  │  Capture   │ ───────► │ Encoder │ ──────► │  (QUIC   │ ─────► │ Decoder  │ ─► Playback
//!  │ loopback / │  frames  │ (Opus)  │ packets │ datagram)│ packets│ + jitter │
//!  │  monitor   │          └─────────┘         └──────────┘  buffer└──────────┘
//!  └────────────┘
//! ```
//!
//! * Capture: WASAPI loopback (Windows) or a PipeWire monitor/virtual sink (Linux).
//! * Transport: [`AudioFrame`](deskoryn_proto::AudioFrame) datagrams — loss is
//!   concealed, never retransmitted, so audio never adds latency to input.
//! * [`JitterBuffer`] absorbs network jitter; its depth is set by the
//!   [`AudioProfile`](deskoryn_core::config::AudioProfile) (tiny for low-latency
//!   calls/gaming, larger for glitch-free music/video).

pub mod jitter;
pub mod platform;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio backend on this platform")]
    NoBackend,
    #[error("requested device not found: {0}")]
    NoDevice(String),
    #[error("codec error: {0}")]
    Codec(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A selectable capture or playback endpoint, surfaced to the UI device pickers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub label: String,
    pub is_default: bool,
}

/// PCM audio source (already resampled to the negotiated rate/channels).
#[async_trait]
pub trait Capture: Send {
    /// Next block of interleaved f32 PCM, or `None` when the device stops.
    async fn next_frame(&mut self) -> Result<Option<Vec<f32>>, AudioError>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u8;
}

/// PCM audio sink.
#[async_trait]
pub trait Playback: Send {
    async fn play(&mut self, pcm: &[f32]) -> Result<(), AudioError>;
}

/// Opus encode/decode. Real impl behind the `opus` feature; default build uses a
/// pass-through that lets the pipeline run end-to-end in tests.
pub trait Codec: Send {
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError>;
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError>;
    /// Generate concealment audio for a lost packet (Opus PLC).
    fn conceal(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError>;
}
