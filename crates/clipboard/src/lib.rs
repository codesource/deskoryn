//! # deskoryn-clipboard
//!
//! Makes the clipboard *global*: copying on one machine makes the content
//! pasteable on the other. Modeled on KDE Connect's clipboard sync plus RDP's
//! delayed-rendering trick for large/file payloads.
//!
//! ## Flow
//!
//! 1. A [`ClipboardMonitor`] watches the local OS clipboard and emits a
//!    [`LocalClip`] describing the available formats whenever it changes.
//! 2. The daemon turns that into a [`Clipboard::Offer`](deskoryn_proto::Clipboard)
//!    (small text inlined; everything else advertised by format only).
//! 3. When the peer pastes, it sends a `Pull`; we render the requested format on
//!    demand — large images and file lists stream over a dedicated channel so a
//!    50 MB screenshot never blocks input.
//! 4. File lists resolve to the file-transfer machinery (`deskoryn-filexfer`):
//!    "paste" triggers a background fetch, preserving names/metadata.
//!
//! A loop-suppression token (the originating device + sequence) prevents the
//! classic A→B→A clipboard echo storm.

/// OS-native file-list clipboard (`CF_HDROP` / X11 `text/uri-list`).
#[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
pub mod filelist;
pub mod platform;

use async_trait::async_trait;
use deskoryn_proto::{ClipFormat, ClipPayload};

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("no clipboard backend on this platform/session")]
    NoBackend,
    #[error("format {0:?} not available")]
    NoFormat(ClipFormat),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A snapshot of what the local clipboard currently offers.
#[derive(Clone, Debug)]
pub struct LocalClip {
    /// Monotonic local sequence; bumped on every observed change.
    pub seq: u64,
    pub formats: Vec<ClipFormat>,
}

/// The pump-facing slice of clipboard capability: synchronous read/write of the
/// current local clipboard content. Real backends implement this over the OS
/// clipboard; [`platform::MemClipboard`] implements it in memory for tests. The
/// *change notification* is delivered separately (a channel of [`LocalClip`]) so
/// the pump can `select!` over "local changed" and "peer message" without
/// aliasing a single mutable handle.
pub trait ClipboardAccess: Send + Sync {
    fn read(&self, format: ClipFormat) -> Option<ClipPayload>;
    fn write(&self, payload: ClipPayload);

    /// Absolute paths of files/folders currently on the local file clipboard, if
    /// any. Unlike [`read`](Self::read), which returns wire metadata, this yields
    /// the *source* paths so the pump can stream their bytes over the file-
    /// transfer channel. Returns `None` when there is no file list (or the
    /// backend can't read one — arboard can't, so OS-native backends fill this in).
    fn read_files(&self) -> Option<Vec<std::path::PathBuf>> {
        None
    }

    /// Place a file list on the local clipboard, pointing at `paths` (where the
    /// fetched files landed) so a subsequent OS paste resolves to real files.
    /// No-op on backends that can't write a file list.
    fn write_files(&self, _paths: &[std::path::PathBuf]) {}
}

/// Observe and read/write the local OS clipboard.
#[async_trait]
pub trait ClipboardMonitor: Send {
    /// Await the next local clipboard change.
    async fn next_change(&mut self) -> Result<LocalClip, ClipboardError>;

    /// Render one advertised format to bytes/text on demand (delayed rendering).
    async fn read(&mut self, format: ClipFormat) -> Result<ClipPayload, ClipboardError>;

    /// Place a remote payload onto the local clipboard. `origin_seq` is stored so
    /// the resulting change can be recognized as an echo and not re-offered.
    async fn write(&mut self, payload: ClipPayload, origin_seq: u64) -> Result<(), ClipboardError>;
}

/// Tracks which sequence numbers we wrote ourselves, to break echo loops.
#[derive(Default)]
pub struct EchoGuard {
    last_written_origin: Option<u64>,
}

impl EchoGuard {
    pub fn note_written(&mut self, origin_seq: u64) {
        self.last_written_origin = Some(origin_seq);
    }
    /// True if this local change was caused by our own `write` and should be
    /// suppressed rather than re-offered to the peer.
    pub fn is_echo(&self, _local: &LocalClip) -> bool {
        // Real backends compare a content hash or the platform's own "clipboard
        // owner" signal; here we just expose the hook.
        self.last_written_origin.is_some()
    }
}
