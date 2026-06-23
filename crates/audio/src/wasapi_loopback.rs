//! Windows WASAPI **loopback** capture — grabbing "what's playing" from the
//! default render endpoint, which is what audio forwarding actually needs (and
//! which cpal's default-input path can't do; that captures the mic).
//!
//! WASAPI/COM objects are apartment-bound and `!Send`, so all of it lives on a
//! dedicated thread that initializes COM, opens the render endpoint in loopback
//! mode, and pushes interleaved f32 PCM into a channel the async [`Capture`]
//! awaits. Dropping the capture signals the thread to stop and uninitialize COM.
//!
//! Compile-verified via the `x86_64-pc-windows-gnu` cross target; needs the real
//! Windows box for runtime validation.

use crate::{AudioError, Capture};
use async_trait::async_trait;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

// Pulled in as literals to avoid churn across `windows`-crate constant
// relocations: loopback stream flag and the "buffer is silent" flag.
const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
/// 1 second of buffer, in 100-ns reference-time units.
const REFTIMES_PER_SEC: i64 = 10_000_000;

fn comerr(e: windows::core::Error) -> AudioError {
    AudioError::Backend(format!("wasapi: {e}"))
}

pub struct WasapiLoopbackCapture {
    rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    sample_rate: u32,
    channels: u8,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WasapiLoopbackCapture {
    pub fn open() -> Result<Self, AudioError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(u32, u8), AudioError>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        let handle = std::thread::spawn(move || capture_thread(tx, thread_stop, ready_tx));

        match ready_rx.recv() {
            Ok(Ok((sample_rate, channels))) => Ok(Self {
                rx,
                sample_rate,
                channels,
                stop,
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => Err(AudioError::Backend("wasapi capture thread died".into())),
        }
    }
}

impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[async_trait]
impl Capture for WasapiLoopbackCapture {
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

fn capture_thread(
    tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(u32, u8), AudioError>>,
) {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready.send(Err(comerr(e)));
            return;
        }

        // Open the default render endpoint in loopback mode.
        let setup = (|| -> Result<(IAudioClient, IAudioCaptureClient, u32, u8, u16), AudioError> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(comerr)?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(comerr)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(comerr)?;

            let pwfx = client.GetMixFormat().map_err(comerr)?;
            let wf: &WAVEFORMATEX = &*pwfx;
            let (rate, channels, bits) = (wf.nSamplesPerSec, wf.nChannels as u8, wf.wBitsPerSample);

            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                REFTIMES_PER_SEC,
                0,
                pwfx,
                None,
            );
            CoTaskMemFree(Some(pwfx as *const c_void));
            init.map_err(comerr)?;

            let capture: IAudioCaptureClient = client.GetService().map_err(comerr)?;
            client.Start().map_err(comerr)?;
            Ok((client, capture, rate, channels, bits))
        })();

        let (client, capture, rate, channels, bits) = match setup {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.send(Err(e));
                CoUninitialize();
                return;
            }
        };
        let _ = ready.send(Ok((rate, channels)));

        let chans = channels as usize;
        while !stop.load(Ordering::SeqCst) {
            // Drain all currently-available packets, then idle briefly.
            loop {
                let packet = match capture.GetNextPacketSize() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                if packet == 0 {
                    break;
                }
                let mut pdata: *mut u8 = std::ptr::null_mut();
                let mut nframes: u32 = 0;
                let mut flags: u32 = 0;
                if capture
                    .GetBuffer(&mut pdata, &mut nframes, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                let n = nframes as usize * chans;
                let mut frame = Vec::with_capacity(n);
                if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                    frame.resize(n, 0.0);
                } else if bits == 32 && !pdata.is_null() {
                    frame.extend_from_slice(std::slice::from_raw_parts(pdata as *const f32, n));
                } else if bits == 16 && !pdata.is_null() {
                    let s = std::slice::from_raw_parts(pdata as *const i16, n);
                    frame.extend(s.iter().map(|&x| x as f32 / 32768.0));
                }
                let _ = capture.ReleaseBuffer(nframes);
                if !frame.is_empty() {
                    // Drop rather than block the COM thread if the consumer lags.
                    let _ = tx.try_send(frame);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let _ = client.Stop();
        CoUninitialize();
    }
}
