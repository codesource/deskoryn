//! Real OS audio I/O via [`cpal`], behind the `linux-backend` / `windows-backend`
//! features.
//!
//! cpal's `Stream` is `!Send`, so each stream is built and owned on its own
//! dedicated thread; the async [`Capture`]/[`Playback`] structs hold only
//! `Send` channel ends plus a shutdown signal (dropping the struct stops the
//! thread, which drops the stream).
//!
//! * **Capture** — an input stream (a source, or a sink's `.monitor` source for
//!   loopback) whose callback pushes interleaved f32 PCM blocks into a bounded
//!   channel that `next_frame` awaits.
//! * **Playback** — an output stream whose callback drains a shared queue that
//!   `play` fills.
//!
//! Sample formats other than f32 (i16/u16) are converted at the edge so the rest
//! of the pipeline only ever sees f32.

use crate::{AudioDevice, AudioError, Capture, Playback};
use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

fn backend_err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Backend(e.to_string())
}

/// Enumerate input devices (sources, including monitors) for the UI picker.
pub fn capture_devices() -> Vec<AudioDevice> {
    devices(true)
}

/// Enumerate output devices (sinks).
pub fn playback_devices() -> Vec<AudioDevice> {
    devices(false)
}

fn devices(input: bool) -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default_name = if input {
        host.default_input_device().and_then(|d| d.name().ok())
    } else {
        host.default_output_device().and_then(|d| d.name().ok())
    };
    let iter = if input {
        host.input_devices().ok()
    } else {
        host.output_devices().ok()
    };
    let mut out = Vec::new();
    if let Some(devs) = iter {
        for d in devs {
            if let Ok(name) = d.name() {
                let is_default = Some(&name) == default_name.as_ref();
                out.push(AudioDevice {
                    id: name.clone(),
                    label: name,
                    is_default,
                });
            }
        }
    }
    out
}

fn find_device<I: Iterator<Item = cpal::Device>>(
    devices: Option<I>,
    default: Option<cpal::Device>,
    id: Option<&str>,
) -> Result<cpal::Device, AudioError> {
    match id {
        Some(want) => devices
            .into_iter()
            .flatten()
            .find(|d| d.name().map(|n| n == want).unwrap_or(false))
            .ok_or_else(|| AudioError::NoDevice(want.to_string())),
        None => default.ok_or(AudioError::NoBackend),
    }
}

// --- Capture ----------------------------------------------------------------

pub struct CpalCapture {
    rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    sample_rate: u32,
    channels: u8,
    // Dropping this stops the owning thread, which drops the cpal stream.
    _stop: StopHandle,
}

impl CpalCapture {
    /// Open an input device by name (or the default when `id` is `None`).
    pub fn open(id: Option<&str>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = find_device(host.input_devices().ok(), host.default_input_device(), id)?;
        let supported = device.default_input_config().map_err(backend_err)?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as u8;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        // Bounded so a stalled consumer applies backpressure instead of growing
        // unbounded; audio that can't keep up is better dropped than delayed.
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);
        let stop = spawn_stream_thread(move || {
            let err_fn = |e| tracing::warn!(error = %e, "audio capture stream error");
            let stream = match format {
                cpal::SampleFormat::F32 => {
                    let tx = tx.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            let _ = tx.try_send(data.to_vec());
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let tx = tx.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            let _ = tx.try_send(data.iter().map(|&s| s as f32 / 32768.0).collect());
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let tx = tx.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            let _ = tx.try_send(
                                data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect(),
                            );
                        },
                        err_fn,
                        None,
                    )
                }
                other => return Err(AudioError::Backend(format!("unsupported sample format {other:?}"))),
            };
            stream.map_err(backend_err)
        })?;

        Ok(Self {
            rx,
            sample_rate,
            channels,
            _stop: stop,
        })
    }
}

#[async_trait]
impl Capture for CpalCapture {
    async fn next_frame(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
        Ok(self.rx.recv().await)
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u8 {
        self.channels
    }
}

// --- Playback ---------------------------------------------------------------

pub struct CpalPlayback {
    queue: Arc<Mutex<VecDeque<f32>>>,
    _stop: StopHandle,
}

impl CpalPlayback {
    pub fn open(id: Option<&str>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = find_device(host.output_devices().ok(), host.default_output_device(), id)?;
        let supported = device.default_output_config().map_err(backend_err)?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let q = queue.clone();
        let stop = spawn_stream_thread(move || {
            let err_fn = |e| tracing::warn!(error = %e, "audio playback stream error");
            let stream = match format {
                cpal::SampleFormat::F32 => device.build_output_stream(
                    &config,
                    move |out: &mut [f32], _| {
                        let mut q = q.lock().unwrap();
                        for s in out.iter_mut() {
                            *s = q.pop_front().unwrap_or(0.0);
                        }
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => device.build_output_stream(
                    &config,
                    move |out: &mut [i16], _| {
                        let mut q = q.lock().unwrap();
                        for s in out.iter_mut() {
                            let v = q.pop_front().unwrap_or(0.0);
                            *s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::U16 => device.build_output_stream(
                    &config,
                    move |out: &mut [u16], _| {
                        let mut q = q.lock().unwrap();
                        for s in out.iter_mut() {
                            let v = q.pop_front().unwrap_or(0.0);
                            *s = ((v.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                        }
                    },
                    err_fn,
                    None,
                ),
                other => return Err(AudioError::Backend(format!("unsupported sample format {other:?}"))),
            };
            stream.map_err(backend_err)
        })?;

        Ok(Self { queue, _stop: stop })
    }
}

#[async_trait]
impl Playback for CpalPlayback {
    async fn play(&mut self, pcm: &[f32]) -> Result<(), AudioError> {
        let mut q = self.queue.lock().unwrap();
        // Cap latency: if the sink falls badly behind, drop the oldest audio
        // rather than let the queue (and delay) grow without bound.
        const MAX_QUEUED: usize = 48_000 * 2; // ~1 s stereo @ 48 kHz
        if q.len() > MAX_QUEUED {
            let overflow = q.len() - MAX_QUEUED;
            q.drain(..overflow);
        }
        q.extend(pcm.iter().copied());
        Ok(())
    }
}

// --- Stream thread plumbing -------------------------------------------------

/// Dropping this signals the stream thread to exit (which drops the `!Send`
/// cpal stream on the thread that created it).
struct StopHandle {
    flag: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StopHandle {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.flag;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Build a cpal stream on a dedicated thread (cpal `Stream` is `!Send`), start
/// it, and park the thread until the returned handle is dropped. The builder
/// returns the stream so build errors surface synchronously to the caller.
fn spawn_stream_thread<F>(build: F) -> Result<StopHandle, AudioError>
where
    F: FnOnce() -> Result<cpal::Stream, AudioError> + Send + 'static,
{
    let flag = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_flag = flag.clone();
    // One-shot channel to report the build result back to the caller.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), AudioError>>();

    let handle = std::thread::spawn(move || {
        let stream = match build() {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        if let Err(e) = stream.play() {
            let _ = ready_tx.send(Err(backend_err(e)));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        // Keep the stream alive until asked to stop.
        let (lock, cvar) = &*thread_flag;
        let mut stop = lock.lock().unwrap();
        while !*stop {
            stop = cvar.wait(stop).unwrap();
        }
        // `stream` drops here, on its owning thread.
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(StopHandle {
            flag,
            handle: Some(handle),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err(AudioError::Backend("stream thread died during setup".into())),
    }
}
