//! Clipboard backend selection plus a portable in-memory backend.
//!
//! Real backends:
//! * Linux/Wayland: `wlr-data-control` (wlroots) or the clipboard portal; X11:
//!   ICCCM selections (`CLIPBOARD`) with `INCR` for large transfers.
//! * Windows: the Clipboard API (`OpenClipboard`/`SetClipboardData`) with
//!   delayed rendering (`WM_RENDERFORMAT`) and `CF_HDROP` for file lists.

use crate::{ClipboardError, ClipboardMonitor, LocalClip};
use async_trait::async_trait;
use deskoryn_proto::{ClipFormat, ClipPayload};

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
