//! Audio backend selection, device enumeration, and a pass-through codec.

use crate::{AudioDevice, AudioError, Codec};

/// Enumerate capture devices (sources). Real backends query PipeWire/WASAPI; the
/// default build returns just a synthetic "default" so the UI has something.
pub fn capture_devices() -> Vec<AudioDevice> {
    vec![AudioDevice {
        id: "default".into(),
        label: "System default".into(),
        is_default: true,
    }]
}

/// Enumerate playback devices (sinks).
pub fn playback_devices() -> Vec<AudioDevice> {
    vec![AudioDevice {
        id: "default".into(),
        label: "System default".into(),
        is_default: true,
    }]
}

/// A no-op codec that copies PCM<->bytes so the streaming pipeline runs end to
/// end without the Opus dependency. Replaced by the real Opus codec under the
/// `opus` feature.
pub struct PassthroughCodec;

impl Codec for PassthroughCodec {
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError> {
        let mut out = Vec::with_capacity(pcm.len() * 4);
        for s in pcm {
            out.extend_from_slice(&s.to_le_bytes());
        }
        Ok(out)
    }

    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError> {
        out.clear();
        for c in packet.chunks_exact(4) {
            out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        Ok(())
    }

    fn conceal(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError> {
        // Simplest concealment: a frame of silence. Opus does far better.
        for s in out.iter_mut() {
            *s = 0.0;
        }
        Ok(())
    }
}

// Real backends, gated by feature + target_os.
//
// #[cfg(all(target_os = "linux", feature = "linux-backend"))]
// mod pipewire { /* monitor-source capture; create a virtual sink to expose a
//                  "Deskoryn" output device the user can select system-wide */ }
//
// #[cfg(all(target_os = "windows", feature = "windows-backend"))]
// mod wasapi { /* IAudioClient loopback capture in shared mode; IAudioRenderClient
//               for playback; AUDCLNT_STREAMFLAGS_EVENTCALLBACK for low latency */ }
//
// #[cfg(feature = "opus")]
// mod opus_codec { /* audiopus::{Encoder, Decoder}; set application to Voip for
//                    low-latency, Audio for high-quality; enable FEC + DTX */ }
