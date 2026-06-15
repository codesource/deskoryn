//! Clipboard backend selection plus a portable in-memory backend.
//!
//! Real backends:
//! * Linux/Wayland: `wlr-data-control` (wlroots) or the clipboard portal; X11:
//!   ICCCM selections (`CLIPBOARD`) with `INCR` for large transfers.
//! * Windows: the Clipboard API (`OpenClipboard`/`SetClipboardData`) with
//!   delayed rendering (`WM_RENDERFORMAT`) and `CF_HDROP` for file lists.

use crate::{ClipboardAccess, ClipboardError, ClipboardMonitor, LocalClip};
use async_trait::async_trait;
use deskoryn_proto::{ClipFormat, ClipPayload};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;

/// In-memory [`ClipboardAccess`] backing the pump in tests and `--dry-run`.
pub struct MemClipboard {
    shared: Arc<StdMutex<Option<ClipPayload>>>,
}

impl ClipboardAccess for MemClipboard {
    fn read(&self, format: ClipFormat) -> Option<ClipPayload> {
        let cur = self.shared.lock().unwrap().clone()?;
        let matches = matches!(
            (format, &cur),
            (ClipFormat::Utf8Text, ClipPayload::Text(_))
                | (ClipFormat::Html, ClipPayload::Html(_))
                | (ClipFormat::Png, ClipPayload::Bytes(_))
                | (ClipFormat::FileList, ClipPayload::Files(_))
        );
        matches.then_some(cur)
    }

    fn write(&self, payload: ClipPayload) {
        *self.shared.lock().unwrap() = Some(payload);
    }
}

/// Simulates local copies (and inspects the current content) for the in-memory
/// clipboard, sharing state with the [`MemClipboard`] from the same [`memory`] call.
pub struct ClipInjector {
    shared: Arc<StdMutex<Option<ClipPayload>>>,
    tx: mpsc::UnboundedSender<LocalClip>,
    seq: AtomicU64,
}

impl ClipInjector {
    /// Simulate a local "copy text", notifying any watcher.
    pub fn copy_text(&self, text: impl Into<String>) {
        *self.shared.lock().unwrap() = Some(ClipPayload::Text(text.into()));
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(LocalClip {
            seq,
            formats: vec![ClipFormat::Utf8Text],
        });
    }

    /// The content currently on this (in-memory) clipboard.
    pub fn current(&self) -> Option<ClipPayload> {
        self.shared.lock().unwrap().clone()
    }
}

/// Build an in-memory clipboard: a shared [`ClipboardAccess`], an injector to
/// drive/inspect it, and the change stream the pump watches.
pub fn memory() -> (Arc<MemClipboard>, ClipInjector, mpsc::UnboundedReceiver<LocalClip>) {
    let shared = Arc::new(StdMutex::new(None));
    let (tx, rx) = mpsc::unbounded_channel();
    let access = Arc::new(MemClipboard { shared: shared.clone() });
    let injector = ClipInjector {
        shared,
        tx,
        seq: AtomicU64::new(0),
    };
    (access, injector, rx)
}

/// An in-memory clipboard used in tests and `--dry-run`. Also a reference for
/// the trait semantics.
#[derive(Default)]
pub struct MemoryClipboard {
    seq: u64,
    text: Option<String>,
    change_tx: Option<tokio::sync::mpsc::UnboundedSender<LocalClip>>,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a local copy (used in tests).
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.seq += 1;
        self.text = Some(text.into());
        if let Some(tx) = &self.change_tx {
            let _ = tx.send(LocalClip {
                seq: self.seq,
                formats: vec![ClipFormat::Utf8Text],
            });
        }
    }
}

#[async_trait]
impl ClipboardMonitor for MemoryClipboard {
    async fn next_change(&mut self) -> Result<LocalClip, ClipboardError> {
        // The memory backend only changes via `set_text`; in `--dry-run` there is
        // no external source, so this parks until the process exits.
        std::future::pending().await
    }

    async fn read(&mut self, format: ClipFormat) -> Result<ClipPayload, ClipboardError> {
        match format {
            ClipFormat::Utf8Text => self
                .text
                .clone()
                .map(ClipPayload::Text)
                .ok_or(ClipboardError::NoFormat(format)),
            other => Err(ClipboardError::NoFormat(other)),
        }
    }

    async fn write(&mut self, payload: ClipPayload, _origin_seq: u64) -> Result<(), ClipboardError> {
        if let ClipPayload::Text(t) = payload {
            self.text = Some(t);
        }
        Ok(())
    }
}

/// Open the best clipboard backend for this platform (currently the memory
/// backend until the OS backends land behind their features).
pub fn open() -> Result<Box<dyn ClipboardMonitor>, ClipboardError> {
    Ok(Box::new(MemoryClipboard::new()))
}

/// A no-op [`ClipboardAccess`] for the portable build / `--dry-run`: it never
/// observes or renders anything. It holds the change-stream sender so the pump's
/// change stream parks forever instead of seeing EOF and tearing the session down.
pub struct IdleClipboard {
    _keep_change_tx: mpsc::UnboundedSender<LocalClip>,
}

impl ClipboardAccess for IdleClipboard {
    fn read(&self, _format: ClipFormat) -> Option<ClipPayload> {
        None
    }
    fn write(&self, _payload: ClipPayload) {}
}

/// The pump-facing entry point: a [`ClipboardAccess`] over the local clipboard
/// plus the change stream the pump watches.
///
/// With a backend feature enabled this is the real OS clipboard (polled every
/// `poll`, text today; echo-suppressed). Otherwise it is an idle no-op backend
/// so the default/portable build and `--dry-run` still wire the pump without
/// touching any real clipboard.
pub fn open_access(
    poll: std::time::Duration,
) -> (Arc<dyn ClipboardAccess>, mpsc::UnboundedReceiver<LocalClip>) {
    #[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
    {
        match system::watch(poll) {
            Ok((access, rx)) => return (access as Arc<dyn ClipboardAccess>, rx),
            Err(e) => tracing::warn!(error = %e, "system clipboard unavailable; using idle backend"),
        }
    }
    let _ = poll;
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(IdleClipboard { _keep_change_tx: tx }), rx)
}

/// Real OS clipboard via `arboard`, behind the `*-backend` features.
///
/// Compile-verified; runtime needs a desktop session (X11/Wayland display or the
/// Windows clipboard). Text is fully supported; image/file formats are a TODO.
#[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
pub mod system {
    use super::*;

    /// [`ClipboardAccess`] over the OS clipboard, backed by **one** long-lived
    /// `arboard::Clipboard`.
    ///
    /// A persistent handle matters on X11: the selection's owner *is* the
    /// `Clipboard` instance, so a fresh-per-call handle drops ownership the
    /// instant it returns and the content only survives if a clipboard manager
    /// happens to grab it (arboard warns about exactly this). Holding one handle
    /// for the backend's lifetime keeps written content available, and also
    /// avoids opening a new X11 connection on every poll.
    ///
    /// Echo-suppressed: the content we `write` is remembered so the poller (see
    /// [`watch`]) doesn't re-offer it back to the peer.
    pub struct SystemClipboard {
        cb: Arc<StdMutex<arboard::Clipboard>>,
        last_written: Arc<StdMutex<Option<String>>>,
    }

    impl ClipboardAccess for SystemClipboard {
        fn read(&self, format: ClipFormat) -> Option<ClipPayload> {
            match format {
                ClipFormat::Utf8Text => self.cb.lock().unwrap().get_text().ok().map(ClipPayload::Text),
                // TODO(impl): images via arboard get_image() (RGBA <-> PNG with the
                // `image` crate); file lists via the OS file-clipboard formats.
                _ => None,
            }
        }

        fn write(&self, payload: ClipPayload) {
            if let ClipPayload::Text(text) = payload {
                // Remember the text *before* setting it so the poller can't race
                // in and re-offer our own write as a fresh local change.
                *self.last_written.lock().unwrap() = Some(text.clone());
                if self.cb.lock().unwrap().set_text(text).is_err() {
                    *self.last_written.lock().unwrap() = None;
                }
            }
        }
    }

    /// Build the system clipboard access plus a change stream produced by polling
    /// (arboard exposes no change events). Polling skips content we wrote
    /// ourselves so there is no echo. Fails if no OS clipboard is reachable
    /// (e.g. no display).
    pub fn watch(
        poll: std::time::Duration,
    ) -> Result<(Arc<SystemClipboard>, mpsc::UnboundedReceiver<LocalClip>), ClipboardError> {
        let cb = arboard::Clipboard::new().map_err(|e| ClipboardError::Backend(e.to_string()))?;
        let cb = Arc::new(StdMutex::new(cb));
        let last_written = Arc::new(StdMutex::new(None::<String>));
        let access = Arc::new(SystemClipboard { cb: cb.clone(), last_written: last_written.clone() });
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut last_seen: Option<String> = None;
            let mut seq: u64 = 0;
            loop {
                tokio::time::sleep(poll).await;
                let text = match cb.lock().unwrap().get_text() {
                    Ok(t) => t,
                    Err(_) => continue, // empty or non-text clipboard
                };
                if last_seen.as_deref() == Some(text.as_str()) {
                    continue; // unchanged
                }
                last_seen = Some(text.clone());
                // Suppress our own writes (echo).
                if last_written.lock().unwrap().as_deref() == Some(text.as_str()) {
                    continue;
                }
                seq += 1;
                if tx.send(LocalClip { seq, formats: vec![ClipFormat::Utf8Text] }).is_err() {
                    break;
                }
            }
        });

        Ok((access, rx))
    }
}
