//! `deskorynd input-test` — a standalone input-backend diagnostic.
//!
//! M2's real backends (evdev capture + uinput injection on Linux) can only be
//! validated on hardware with the right device permissions, so they are not
//! covered by the automated test suite. This command exercises them in isolation
//! — no peer, no network — so the user can confirm capture and injection work
//! before relying on a live session:
//!
//! * **capture**: reads (without grabbing, so you keep control of your machine)
//!   and prints every pointer/keyboard event for a few seconds;
//! * **inject** (`--inject`): emits a small cursor wiggle through the virtual
//!   uinput device so you can see injection land locally.
//!
//! With the default (portable) build the backend is `Null` and this command just
//! explains how to get a real one (`--features linux`).

use deskoryn_input::platform::{self, Backend};
use std::time::Duration;

pub async fn input_test(secs: u64, inject: bool) -> anyhow::Result<()> {
    let backend = platform::detect();
    println!("input backend: {backend:?}");

    if backend == Backend::Null {
        println!(
            "this is the portable no-op backend — it never captures or injects.\n\
             rebuild with the OS backend to test real devices, e.g.:\n  \
             cargo run -p deskoryn-daemon --features linux -- input-test"
        );
        return Ok(());
    }

    if inject {
        match platform::open_injector() {
            Ok(mut inj) => {
                println!("injecting a cursor wiggle (watch your pointer)...");
                for _ in 0..20 {
                    inj.inject(deskoryn_core::input::InputEvent::PointerMotion { dx: 8, dy: 0 })
                        .await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                for _ in 0..20 {
                    inj.inject(deskoryn_core::input::InputEvent::PointerMotion { dx: -8, dy: 0 })
                        .await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                inj.release_all().await?;
                println!("injection ok.");
            }
            Err(e) => println!("injector unavailable: {e}"),
        }
    }

    let mut capture = match platform::open_capture() {
        Ok(c) => c,
        Err(e) => {
            println!("capture unavailable: {e}");
            return Ok(());
        }
    };

    println!("reading input for {secs}s (move the mouse / press keys; not grabbed)...");
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    let mut count = 0u64;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = capture.next_event() => {
                match ev {
                    Ok(ev) => {
                        count += 1;
                        // Throttle noisy pointer motion in the printout.
                        if !matches!(ev, deskoryn_core::input::InputEvent::PointerMotion { .. }) || count % 20 == 0 {
                            println!("  {ev:?}");
                        }
                    }
                    Err(e) => {
                        println!("capture ended: {e}");
                        break;
                    }
                }
            }
        }
    }
    println!("captured {count} events.");
    Ok(())
}

/// `deskorynd clip-test` — a standalone clipboard-backend diagnostic.
///
/// Mirrors [`input_test`] for M3: it exercises the real OS clipboard
/// (`open_access`) in isolation — no peer, no network — so each machine can be
/// validated before a live session. With `--set` it writes text first; then it
/// watches for changes for a few seconds (copy something to see it land).
///
/// With the default (portable) build the backend is the idle no-op and this just
/// explains how to get a real one (`--features linux` / `--features windows`).
pub async fn clip_test(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    secs: u64,
    set: Option<String>,
) -> anyhow::Result<()> {
    use deskoryn_proto::{ClipFormat, ClipPayload};

    let real_backend = cfg!(any(feature = "linux", feature = "windows"));
    if !real_backend {
        println!(
            "clipboard backend: idle no-op (portable build) — it never reads or writes.\n\
             rebuild with the OS backend to test the real clipboard, e.g.:\n  \
             cargo run -p deskoryn-daemon --features linux -- clip-test"
        );
        return Ok(());
    }
    println!(
        "clipboard backend: real OS clipboard (arboard text/image + native file-list), polling every {} ms",
        config.clipboard.poll_ms
    );

    let poll = Duration::from_millis(config.clipboard.poll_ms);
    let (access, mut changes) = deskoryn_clipboard::platform::open_access(poll);

    if let Some(text) = set {
        println!("setting clipboard to: {text:?}");
        access.write(ClipPayload::Text(text));
    } else if let Some(ClipPayload::Text(cur)) = access.read(ClipFormat::Utf8Text) {
        println!("current clipboard text: {cur:?}");
    }

    println!("watching for changes for {secs}s (copy text / an image / files to see them detected)...");
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    let mut count = 0u64;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            change = changes.recv() => {
                let Some(change) = change else { break };
                count += 1;
                match change.formats.first() {
                    Some(ClipFormat::FileList) => {
                        let files = access.read_files();
                        println!("  change seq={} formats={:?} files={:?}", change.seq, change.formats, files);
                    }
                    Some(ClipFormat::Png) => {
                        let bytes = access.read(ClipFormat::Png).map(|p| match p {
                            ClipPayload::Bytes(b) => b.len(),
                            _ => 0,
                        });
                        println!("  change seq={} formats={:?} png_bytes={:?}", change.seq, change.formats, bytes);
                    }
                    _ => {
                        let text = access.read(ClipFormat::Utf8Text);
                        println!("  change seq={} formats={:?} text={:?}", change.seq, change.formats, text);
                    }
                }
            }
        }
    }
    println!("observed {count} clipboard change(s).");
    Ok(())
}

/// `deskorynd audio-test` — exercise the local audio backend in isolation.
///
/// Lists the host's capture/playback devices, then (unless `--list`) opens the
/// configured source + sink and forwards captured PCM straight to playback for a
/// few seconds — a *local* loopback through the real backend, no peer or
/// network. Capturing a sink's `.monitor` source lets you hear "what's playing"
/// looped back, which is the same capture path audio forwarding uses.
///
/// With the portable build there is no device I/O (`open_capture` returns
/// `NoBackend`); rebuild with `--features linux` (or `windows`).
pub async fn audio_test(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    secs: u64,
    list: bool,
) -> anyhow::Result<()> {
    use deskoryn_audio::platform::{capture_devices, open_capture, open_playback, playback_devices};
    use std::time::Duration;

    let real_backend = cfg!(any(feature = "linux", feature = "windows"));
    if !real_backend {
        println!(
            "audio backend: none (portable build) — no device capture/playback.\n\
             rebuild with the OS backend to test real audio, e.g.:\n  \
             cargo run -p deskoryn-daemon --features linux -- audio-test"
        );
        return Ok(());
    }

    println!("capture devices (sources / monitors):");
    for d in capture_devices() {
        println!("  {} {}", if d.is_default { "*" } else { " " }, d.label);
    }
    println!("playback devices (sinks):");
    for d in playback_devices() {
        println!("  {} {}", if d.is_default { "*" } else { " " }, d.label);
    }
    if list {
        return Ok(());
    }

    // "default" is the config sentinel for the host default device.
    let as_opt = |s: &str| (s != "default").then(|| s.to_string());
    let src = as_opt(&config.audio.source_device);
    let sink = as_opt(&config.audio.sink_device);

    let mut capture = open_capture(src.as_deref())?;
    let mut playback = open_playback(sink.as_deref())?;
    println!(
        "looping capture → playback for {secs}s at {} Hz / {} ch (you should hear the captured source)…",
        capture.sample_rate(),
        capture.channels()
    );

    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    let mut frames = 0u64;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = capture.next_frame() => {
                match frame? {
                    Some(pcm) => {
                        playback.play(&pcm).await?;
                        frames += 1;
                    }
                    None => break,
                }
            }
        }
    }
    println!("forwarded {frames} audio frame(s).");
    Ok(())
}
