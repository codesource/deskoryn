//! Audio backend selection, device enumeration, and a pass-through codec.

use crate::{AudioDevice, AudioError, Capture, Codec, Playback};
use deskoryn_core::config::AudioProfile;

/// Pick the best codec for this build: the real Opus codec when the `opus`
/// feature is enabled (falling back to passthrough if the encoder can't be
/// created for the given format), otherwise the passthrough codec. This is the
/// single selection point the daemon's audio pump uses.
pub fn open_codec(sample_rate: u32, channels: u8, profile: AudioProfile) -> Box<dyn Codec> {
    #[cfg(feature = "opus")]
    {
        match opus_codec::OpusCodec::new(sample_rate, channels, profile) {
            Ok(c) => return Box::new(c),
            Err(e) => tracing::warn!(error = %e, "opus codec unavailable; using passthrough"),
        }
    }
    let _ = (sample_rate, channels, profile);
    Box::new(PassthroughCodec)
}

/// Enumerate capture devices (sources, including sink monitors for loopback).
/// With a backend feature on, queries the real host (cpal); the default build
/// returns just a synthetic "default" so the UI has something.
pub fn capture_devices() -> Vec<AudioDevice> {
    #[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
    {
        let devs = crate::cpal_backend::capture_devices();
        if !devs.is_empty() {
            return devs;
        }
    }
    vec![AudioDevice {
        id: "default".into(),
        label: "System default".into(),
        is_default: true,
    }]
}

/// Enumerate playback devices (sinks).
pub fn playback_devices() -> Vec<AudioDevice> {
    #[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
    {
        let devs = crate::cpal_backend::playback_devices();
        if !devs.is_empty() {
            return devs;
        }
    }
    vec![AudioDevice {
        id: "default".into(),
        label: "System default".into(),
        is_default: true,
    }]
}

/// Open a capture source (a device id from [`capture_devices`], or the default
/// when `None`). The real backend (cpal) is compiled in behind the per-OS
/// backend feature; the default portable build has no device I/O and returns
/// [`AudioError::NoBackend`]. This is the single selection point the daemon's
/// audio pump uses, mirroring [`open_codec`].
pub fn open_capture(device: Option<&str>) -> Result<Box<dyn Capture>, AudioError> {
    // On Windows, capturing "what's playing" means WASAPI loopback on the default
    // render endpoint (cpal's default input is the mic). Use loopback when no
    // explicit device id is requested; a named id falls through to cpal.
    #[cfg(all(windows, feature = "windows-backend"))]
    {
        if device.is_none() {
            return Ok(Box::new(crate::wasapi_loopback::WasapiLoopbackCapture::open()?));
        }
    }
    #[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
    {
        return Ok(Box::new(crate::cpal_backend::CpalCapture::open(device)?));
    }
    #[cfg(not(any(feature = "linux-backend", feature = "windows-backend")))]
    {
        let _ = device;
        Err(AudioError::NoBackend)
    }
}

/// Open a playback sink (a device id from [`playback_devices`], or the default
/// when `None`). See [`open_capture`].
pub fn open_playback(device: Option<&str>) -> Result<Box<dyn Playback>, AudioError> {
    #[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
    {
        return Ok(Box::new(crate::cpal_backend::CpalPlayback::open(device)?));
    }
    #[cfg(not(any(feature = "linux-backend", feature = "windows-backend")))]
    {
        let _ = device;
        Err(AudioError::NoBackend)
    }
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

/// Real Opus codec (feature = "opus"), wrapping libopus via `audiopus`.
///
/// Low-latency profile uses the VoIP application + small frames; high-quality
/// uses the Audio application. Lost packets are concealed by Opus PLC (decode
/// with `None`). Compile-verified and unit-tested (encode→decode round-trip).
#[cfg(feature = "opus")]
pub mod opus_codec {
    use crate::{AudioError, Codec};
    use audiopus::{coder::Decoder, coder::Encoder, Application, Channels, SampleRate};
    use deskoryn_core::config::AudioProfile;

    pub struct OpusCodec {
        encoder: Encoder,
        decoder: Decoder,
        channels: usize,
        /// Max samples-per-channel a decoded frame may contain (120 ms ceiling).
        max_frame: usize,
    }

    fn rate(hz: u32) -> SampleRate {
        match hz {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            _ => SampleRate::Hz48000,
        }
    }

    fn err(e: audiopus::Error) -> AudioError {
        AudioError::Codec(e.to_string())
    }

    impl OpusCodec {
        pub fn new(sample_rate: u32, channels: u8, profile: AudioProfile) -> Result<Self, AudioError> {
            let ch = if channels <= 1 { Channels::Mono } else { Channels::Stereo };
            let app = match profile {
                AudioProfile::LowLatency => Application::Voip,
                AudioProfile::HighQuality => Application::Audio,
            };
            let encoder = Encoder::new(rate(sample_rate), ch, app).map_err(err)?;
            let decoder = Decoder::new(rate(sample_rate), ch).map_err(err)?;
            Ok(Self {
                encoder,
                decoder,
                channels: ch as usize,
                max_frame: (sample_rate as usize / 1000) * 120, // 120 ms
            })
        }
    }

    impl Codec for OpusCodec {
        fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError> {
            let mut out = vec![0u8; 4000]; // Opus max packet size
            let n = self.encoder.encode_float(pcm, &mut out).map_err(err)?;
            out.truncate(n);
            Ok(out)
        }

        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError> {
            out.clear();
            out.resize(self.max_frame * self.channels, 0.0);
            let n = self
                .decoder
                .decode_float(Some(packet), &mut out[..], false)
                .map_err(err)?;
            out.truncate(n * self.channels);
            Ok(())
        }

        fn conceal(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError> {
            out.clear();
            out.resize(self.max_frame * self.channels, 0.0);
            // Packet-loss concealment: decode with no input.
            let n = self
                .decoder
                .decode_float(None::<&[u8]>, &mut out[..], false)
                .map_err(err)?;
            out.truncate(n * self.channels);
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn opus_round_trip_runs() {
            let mut codec =
                OpusCodec::new(48_000, 2, AudioProfile::LowLatency).expect("create codec");
            // One valid 20 ms stereo frame at 48 kHz = 960 samples/channel.
            let frame: Vec<f32> = (0..960 * 2).map(|i| ((i as f32) * 0.001).sin() * 0.2).collect();
            let packet = codec.encode(&frame).expect("encode");
            assert!(!packet.is_empty(), "opus produced a packet");

            let mut out = Vec::new();
            codec.decode(&packet, &mut out).expect("decode");
            assert_eq!(out.len(), frame.len(), "decoded frame has the same shape");

            // Concealment yields a frame too.
            let mut plc = Vec::new();
            codec.conceal(&mut plc).expect("conceal");
            assert!(!plc.is_empty());
        }
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
