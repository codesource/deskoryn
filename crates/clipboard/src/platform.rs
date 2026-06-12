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
