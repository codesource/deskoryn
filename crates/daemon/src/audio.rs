//! Audio forwarding pump: capture → encode → datagrams → jitter → playback.
//!
//! `AudioControl::Start`/`Stop` ride the reliable Audio channel; the Opus frames
//! ride **QUIC datagrams** (`session.send_datagram`) so loss is concealed and
//! never stalls input/clipboard. See `docs/PROTOCOL.md`.

// The pump logic is exercised by the integration test over the loopback session;
// `run_audio_pump` wires it into the live per-peer session (M5).
#![allow(dead_code)]

use bytes::BytesMut;
use deskoryn_audio::{Capture, Codec, JitterBuffer, Playback};
use deskoryn_core::config::AudioProfile;
use deskoryn_net::transport::{Session, Sink, Source};
use deskoryn_proto::{
    decode_one, encode, from_datagram, to_datagram, AudioControl, AudioFrame, Channel,
};
use std::sync::Arc;

/// Run the audio-forwarding pump for one peer session, in a single direction
/// chosen by config:
///
/// * **source** (`forward` = `config.audio.forward_enabled`): capture this
///   machine's output, re-frame to Opus frames, encode, and stream as datagrams.
/// * **sink** (otherwise, when the peer forwards): play what the peer sends.
///
/// Returns a never-ending future when this machine has no audio role or no
/// backend, so it simply parks in the session's `select!` without ending it.
/// Real forwarding needs the `audio-opus` codec (raw PCM frames don't fit a
/// datagram) and a 48 kHz-capable default device; without them it stays idle.
pub async fn run_audio_pump(
    session: Arc<dyn Session>,
    profile: AudioProfile,
    forward: bool,
    peer_forwards: bool,
) -> anyhow::Result<()> {
    use deskoryn_audio::platform::{open_capture, open_codec, open_playback};
    use deskoryn_audio::reframe::{frame_ms, ReframingCapture};

    if forward {
        let capture = match open_capture(None) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "audio: no capture backend; not forwarding");
                return std::future::pending().await;
            }
        };
        let (rate, channels) = (capture.sample_rate(), capture.channels());
        tracing::info!(rate, channels, ?profile, "audio: forwarding this machine's output");
        let framed: Box<dyn Capture> = Box::new(ReframingCapture::new(capture, profile));
        let codec = open_codec(rate, channels, profile);
        run_audio_source(session.as_ref(), framed, codec, 0, profile, frame_ms(profile) * 1000).await
    } else if peer_forwards {
        let playback = match open_playback(None) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "audio: no playback backend; not receiving");
                return std::future::pending().await;
            }
        };
        tracing::info!("audio: playing peer's forwarded audio");
        run_audio_sink(session.as_ref(), playback, open_codec).await
    } else {
        // Neither side forwards: nothing to do.
        std::future::pending().await
    }
}

/// Source side: announce the stream, then capture → encode → datagram until the
/// capture device stops.
pub async fn run_audio_source(
    session: &dyn Session,
    mut capture: Box<dyn Capture>,
    mut codec: Box<dyn Codec>,
    tag: u64,
    profile: AudioProfile,
    frame_us: u32,
) -> anyhow::Result<()> {
    let (mut ctl, _ctl_rx) = session.channel(Channel::Audio).await?;
    let start = AudioControl::Start {
        tag,
        profile,
        sample_rate: capture.sample_rate(),
        channels: capture.channels(),
        frame_us,
    };
    send_ctl(&mut ctl, &start).await?;

    let mut seq: u32 = 0;
    while let Some(pcm) = capture.next_frame().await? {
        let opus = codec.encode(&pcm)?;
        let dg = to_datagram(&AudioFrame { tag, seq, opus })?;
        // Best-effort: a dropped datagram is concealed by the receiver.
        let _ = session.send_datagram(&dg).await;
        seq = seq.wrapping_add(1);
    }

    send_ctl(&mut ctl, &AudioControl::Stop { tag }).await?;
    Ok(())
}

/// Sink side: receive the announcement, buffer datagrams in a jitter buffer, and
/// play them out (concealing gaps), draining on `Stop`.
///
/// The decoder is built from `Start` (via `make_codec`) because the source's
/// sample rate / channels / profile aren't known until then.
pub async fn run_audio_sink<F>(
    session: &dyn Session,
    mut playback: Box<dyn Playback>,
    make_codec: F,
) -> anyhow::Result<()>
where
    F: FnOnce(u32, u8, AudioProfile) -> Box<dyn Codec>,
{
    let (_ctl_tx, mut ctl_rx) = session.channel(Channel::Audio).await?;

    // Wait for Start to learn the format (codec) and jitter depth (profile).
    let (mut jitter, mut codec) = match recv_ctl(&mut ctl_rx).await? {
        Some(AudioControl::Start { profile, sample_rate, channels, .. }) => (
            JitterBuffer::new(JitterBuffer::depth_for(profile)),
            make_codec(sample_rate, channels, profile),
        ),
        Some(other) => anyhow::bail!("expected Start, got {other:?}"),
        None => return Ok(()),
    };

    let mut pcm = Vec::new();
    loop {
        tokio::select! {
            dg = session.recv_datagram() => {
                let Some(bytes) = dg? else { break; };
                if let Ok(frame) = from_datagram::<AudioFrame>(&bytes) {
                    jitter.push(frame.seq, frame.opus);
                    play_one(&mut jitter, &mut codec, &mut playback, &mut pcm).await?;
                }
            }
            ctl = recv_ctl(&mut ctl_rx) => {
                match ctl? {
                    Some(AudioControl::Stop { .. }) | None => {
                        // `Stop` rides the reliable channel and may overtake the
                        // last datagrams, so drain any still in flight (until a
                        // brief idle) before flushing the jitter buffer.
                        while let Ok(Ok(Some(bytes))) =
                            tokio::time::timeout(std::time::Duration::from_millis(50), session.recv_datagram()).await
                        {
                            if let Ok(frame) = from_datagram::<AudioFrame>(&bytes) {
                                jitter.push(frame.seq, frame.opus);
                                play_one(&mut jitter, &mut codec, &mut playback, &mut pcm).await?;
                            }
                        }
                        while jitter.buffered() > 0 {
                            play_one(&mut jitter, &mut codec, &mut playback, &mut pcm).await?;
                        }
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
    }
    Ok(())
}

async fn play_one(
    jitter: &mut JitterBuffer,
    codec: &mut Box<dyn Codec>,
    playback: &mut Box<dyn Playback>,
    pcm: &mut Vec<u8>,
) -> anyhow::Result<()> {
    use deskoryn_audio::jitter::Pop;
    match jitter.pop() {
        Pop::Packet(opus) => {
            let mut out = Vec::new();
            codec.decode(&opus, &mut out)?;
            let _ = pcm; // reserved for byte-level buffering if needed
            playback.play(&out).await?;
        }
        Pop::Conceal => {
            let mut out = Vec::new();
            codec.conceal(&mut out)?;
            playback.play(&out).await?;
        }
        Pop::Underrun => {}
    }
    Ok(())
}

async fn send_ctl(sink: &mut Box<dyn Sink>, msg: &AudioControl) -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    encode(msg, &mut buf)?;
    sink.send_bytes(&buf).await?;
    Ok(())
}

async fn recv_ctl(source: &mut Box<dyn Source>) -> anyhow::Result<Option<AudioControl>> {
    match source.recv_bytes().await? {
        Some(frame) => {
            let mut b = BytesMut::from(&frame[..]);
            Ok(decode_one::<AudioControl>(&mut b)?)
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use deskoryn_audio::platform::PassthroughCodec;
    use deskoryn_audio::AudioError;
    use deskoryn_core::DeviceId;
    use deskoryn_net::transport::loopback;
    use std::sync::{Arc, Mutex};

    /// Plays a fixed list of PCM frames then stops.
    struct VecCapture {
        frames: std::vec::IntoIter<Vec<f32>>,
    }
    #[async_trait]
    impl Capture for VecCapture {
        async fn next_frame(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
            Ok(self.frames.next())
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u8 {
            2
        }
    }

    /// Collects everything played.
    struct VecPlayback {
        out: Arc<Mutex<Vec<f32>>>,
    }
    #[async_trait]
    impl Playback for VecPlayback {
        async fn play(&mut self, pcm: &[f32]) -> Result<(), AudioError> {
            self.out.lock().unwrap().extend_from_slice(pcm);
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_audio_frames_in_order() {
        // Three distinct stereo frames.
        let frames: Vec<Vec<f32>> = vec![
            vec![0.1, 0.2, 0.3, 0.4],
            vec![0.5, 0.6, 0.7, 0.8],
            vec![0.9, 1.0, -0.1, -0.2],
        ];
        let expected: Vec<f32> = frames.iter().flatten().copied().collect();

        let (src_sess, sink_sess) =
            loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let src_sess: Box<dyn Session> = Box::new(src_sess);
        let sink_sess: Box<dyn Session> = Box::new(sink_sess);

        let out = Arc::new(Mutex::new(Vec::new()));
        let playback = Box::new(VecPlayback { out: out.clone() });

        let sink = tokio::spawn(async move {
            run_audio_sink(sink_sess.as_ref(), playback, |_, _, _| Box::new(PassthroughCodec)).await
        });

        let capture = Box::new(VecCapture { frames: frames.into_iter() });
        run_audio_source(
            src_sess.as_ref(),
            capture,
            Box::new(PassthroughCodec),
            7,
            AudioProfile::LowLatency,
            5_000,
        )
        .await
        .unwrap();

        sink.await.unwrap().unwrap();

        // Passthrough codec + in-order datagrams ⇒ output equals input exactly.
        let got = out.lock().unwrap().clone();
        assert_eq!(got, expected, "played PCM must match captured PCM");
    }

    /// End-to-end through the **real Opus codec** (selected via `open_codec`).
    /// Only built/run with `--features audio-opus` (needs libopus at build time).
    #[cfg(feature = "audio-opus")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_audio_frames_through_opus() {
        use deskoryn_audio::platform::open_codec;

        // Three valid 20 ms stereo frames @ 48 kHz (960 samples/channel).
        let frames: Vec<Vec<f32>> = (0..3)
            .map(|k| (0..960 * 2).map(|i| (((i + k * 7) as f32) * 0.01).sin() * 0.2).collect())
            .collect();
        let n_frames = frames.len();

        let (src_sess, sink_sess) =
            loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let src_sess: Box<dyn Session> = Box::new(src_sess);
        let sink_sess: Box<dyn Session> = Box::new(sink_sess);

        let out = Arc::new(Mutex::new(Vec::new()));
        let playback = Box::new(VecPlayback { out: out.clone() });

        let sink = tokio::spawn(async move {
            // The decoder is built from the Start message's format.
            run_audio_sink(sink_sess.as_ref(), playback, open_codec).await
        });

        let src_codec = open_codec(48_000, 2, AudioProfile::LowLatency);
        let capture = Box::new(VecCapture { frames: frames.into_iter() });
        run_audio_source(src_sess.as_ref(), capture, src_codec, 7, AudioProfile::LowLatency, 20_000)
            .await
            .unwrap();
        sink.await.unwrap().unwrap();

        // Opus is lossy, so output won't equal input — but each 20 ms frame
        // decodes back to 960 samples/channel, so the whole stream survives.
        let got = out.lock().unwrap().len();
        assert!(
            got >= n_frames * 960 * 2,
            "expected at least {} samples through Opus, got {got}",
            n_frames * 960 * 2
        );
    }
}
