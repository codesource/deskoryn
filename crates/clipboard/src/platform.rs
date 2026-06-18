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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;

/// Shared in-memory clipboard state behind both [`MemClipboard`] and its
/// [`ClipInjector`].
#[derive(Default)]
struct MemState {
    /// Current text/image/html content (what `read`/`write` see).
    content: Option<ClipPayload>,
    /// Absolute source paths of a simulated file copy (what `read_files` sees).
    files: Option<Vec<PathBuf>>,
    /// Paths handed to `write_files` — i.e. where a received file-paste landed.
    landed: Option<Vec<PathBuf>>,
}

/// In-memory [`ClipboardAccess`] backing the pump in tests and `--dry-run`.
pub struct MemClipboard {
    shared: Arc<StdMutex<MemState>>,
}

impl ClipboardAccess for MemClipboard {
    fn read(&self, format: ClipFormat) -> Option<ClipPayload> {
        let cur = self.shared.lock().unwrap().content.clone()?;
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
        self.shared.lock().unwrap().content = Some(payload);
    }

    fn read_files(&self) -> Option<Vec<PathBuf>> {
        self.shared.lock().unwrap().files.clone()
    }

    fn write_files(&self, paths: &[PathBuf]) {
        self.shared.lock().unwrap().landed = Some(paths.to_vec());
    }
}

/// Simulates local copies (and inspects the current content) for the in-memory
/// clipboard, sharing state with the [`MemClipboard`] from the same [`memory`] call.
pub struct ClipInjector {
    shared: Arc<StdMutex<MemState>>,
    tx: mpsc::UnboundedSender<LocalClip>,
    seq: AtomicU64,
}

impl ClipInjector {
    /// Simulate a local "copy text", notifying any watcher.
    pub fn copy_text(&self, text: impl Into<String>) {
        self.shared.lock().unwrap().content = Some(ClipPayload::Text(text.into()));
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(LocalClip {
            seq,
            formats: vec![ClipFormat::Utf8Text],
        });
    }

    /// Simulate a local "copy image" (PNG bytes), notifying any watcher.
    pub fn copy_image(&self, bytes: Vec<u8>) {
        self.shared.lock().unwrap().content = Some(ClipPayload::Bytes(bytes));
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(LocalClip {
            seq,
            formats: vec![ClipFormat::Png],
        });
    }

    /// Simulate a local "copy files", notifying any watcher with a `FileList`.
    pub fn copy_files(&self, paths: Vec<PathBuf>) {
        self.shared.lock().unwrap().files = Some(paths);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(LocalClip {
            seq,
            formats: vec![ClipFormat::FileList],
        });
    }

    /// The content currently on this (in-memory) clipboard.
    pub fn current(&self) -> Option<ClipPayload> {
        self.shared.lock().unwrap().content.clone()
    }

    /// Where a received file-paste landed (what `write_files` recorded), if any.
    pub fn landed_files(&self) -> Option<Vec<PathBuf>> {
        self.shared.lock().unwrap().landed.clone()
    }
}

/// Build an in-memory clipboard: a shared [`ClipboardAccess`], an injector to
/// drive/inspect it, and the change stream the pump watches.
pub fn memory() -> (Arc<MemClipboard>, ClipInjector, mpsc::UnboundedReceiver<LocalClip>) {
    let shared = Arc::new(StdMutex::new(MemState::default()));
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
/// `poll`, text + images; echo-suppressed). Otherwise it is an idle no-op backend
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
/// Windows clipboard). Text and images (PNG on the wire) are supported; file
/// lists still need OS-native formats (`CF_HDROP` / `text/uri-list`), which
/// arboard does not expose.
#[cfg(any(feature = "linux-backend", feature = "windows-backend"))]
pub mod system {
    use super::*;
    use std::hash::{Hash, Hasher};

    /// Content hash of an offerable clipboard item, computed over the *canonical
    /// clipboard representation* (the text string, or the decoded RGBA pixels) —
    /// not the wire payload. PNG re-encoding is not byte-stable, so hashing the
    /// PNG bytes would defeat image echo-suppression; hashing the RGBA the
    /// clipboard actually holds round-trips exactly.
    fn text_hash(s: &str) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        0u8.hash(&mut h);
        s.hash(&mut h);
        h.finish()
    }

    fn image_hash(width: usize, height: usize, rgba: &[u8]) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        1u8.hash(&mut h);
        width.hash(&mut h);
        height.hash(&mut h);
        rgba.hash(&mut h);
        h.finish()
    }

    fn files_hash(paths: &[std::path::PathBuf]) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        2u8.hash(&mut h);
        for p in paths {
            p.hash(&mut h);
        }
        h.finish()
    }

    /// Encode arboard's RGBA8 image to PNG bytes for the wire.
    fn encode_png(img: &arboard::ImageData) -> Option<Vec<u8>> {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder};
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(&img.bytes, img.width as u32, img.height as u32, ExtendedColorType::Rgba8)
            .ok()?;
        Some(out)
    }

    /// Decode wire PNG bytes back to arboard's RGBA8 image.
    fn decode_png(bytes: &[u8]) -> Option<arboard::ImageData<'static>> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Some(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        })
    }

    /// What the OS clipboard currently offers, as `(format, content hash)`. Text
    /// wins over an image when both are present (the common case after copying
    /// text). The hash drives change detection in [`watch`] without rendering or
    /// encoding anything — the bytes are produced lazily in [`SystemClipboard::read`].
    fn probe(cb: &StdMutex<arboard::Clipboard>) -> Option<(ClipFormat, u64)> {
        {
            let mut g = cb.lock().unwrap();
            if let Ok(t) = g.get_text() {
                if !t.is_empty() {
                    return Some((ClipFormat::Utf8Text, text_hash(&t)));
                }
            }
            if let Ok(img) = g.get_image() {
                return Some((ClipFormat::Png, image_hash(img.width, img.height, &img.bytes)));
            }
        }
        // A file copy (handled by the OS-native file-list backend, not arboard).
        if let Some(paths) = crate::filelist::read() {
            if !paths.is_empty() {
                return Some((ClipFormat::FileList, files_hash(&paths)));
            }
        }
        None
    }

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
    /// Echo-suppressed: the content we `write` is remembered (as a content hash)
    /// so the poller (see [`watch`]) doesn't re-offer it back to the peer.
    pub struct SystemClipboard {
        cb: Arc<StdMutex<arboard::Clipboard>>,
        last_written: Arc<StdMutex<Option<u64>>>,
        /// Holds the X11 file-selection owner (Linux) for as long as our file
        /// list should stay on the clipboard; inert on Windows. Replacing it
        /// drops the previous owner, relinquishing the old selection.
        file_owner: Arc<StdMutex<Option<crate::filelist::OwnerGuard>>>,
    }

    impl ClipboardAccess for SystemClipboard {
        fn read(&self, format: ClipFormat) -> Option<ClipPayload> {
            let mut g = self.cb.lock().unwrap();
            match format {
                ClipFormat::Utf8Text => g.get_text().ok().map(ClipPayload::Text),
                ClipFormat::Png => {
                    let img = g.get_image().ok()?;
                    encode_png(&img).map(ClipPayload::Bytes)
                }
                // File lists need OS-native formats arboard can't reach.
                _ => None,
            }
        }

        fn read_files(&self) -> Option<Vec<std::path::PathBuf>> {
            crate::filelist::read()
        }

        fn write_files(&self, paths: &[std::path::PathBuf]) {
            // Remember the hash so the poller doesn't re-offer our own write as a
            // fresh local file copy (echo). Then take selection ownership; the
            // previous owner (if any) is dropped, relinquishing the old list.
            *self.last_written.lock().unwrap() = Some(files_hash(paths));
            let owner = crate::filelist::write(paths);
            *self.file_owner.lock().unwrap() = owner;
        }

        fn write(&self, payload: ClipPayload) {
            // Remember the content hash *before* setting it so the poller can't
            // race in and re-offer our own write as a fresh local change.
            match payload {
                ClipPayload::Text(text) => {
                    *self.last_written.lock().unwrap() = Some(text_hash(&text));
                    if self.cb.lock().unwrap().set_text(text).is_err() {
                        *self.last_written.lock().unwrap() = None;
                    }
                }
                ClipPayload::Bytes(png) => {
                    let Some(img) = decode_png(&png) else { return };
                    *self.last_written.lock().unwrap() =
                        Some(image_hash(img.width, img.height, &img.bytes));
                    if self.cb.lock().unwrap().set_image(img).is_err() {
                        *self.last_written.lock().unwrap() = None;
                    }
                }
                _ => {}
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
        let last_written = Arc::new(StdMutex::new(None::<u64>));
        let access = Arc::new(SystemClipboard {
            cb: cb.clone(),
            last_written: last_written.clone(),
            file_owner: Arc::new(StdMutex::new(None)),
        });
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut last_seen: Option<u64> = None;
            let mut seq: u64 = 0;
            loop {
                tokio::time::sleep(poll).await;
                let Some((fmt, hash)) = probe(&cb) else { continue };
                if last_seen == Some(hash) {
                    continue; // unchanged
                }
                last_seen = Some(hash);
                // Suppress our own writes (echo).
                if *last_written.lock().unwrap() == Some(hash) {
                    continue;
                }
                seq += 1;
                if tx.send(LocalClip { seq, formats: vec![fmt] }).is_err() {
                    break;
                }
            }
        });

        Ok((access, rx))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn sample(width: usize, height: usize) -> arboard::ImageData<'static> {
            // A deterministic RGBA gradient so the PNG has real, varied content.
            let mut bytes = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                for x in 0..width {
                    bytes.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0xff]);
                }
            }
            arboard::ImageData { width, height, bytes: std::borrow::Cow::Owned(bytes) }
        }

        #[test]
        fn png_round_trips_pixels() {
            let img = sample(7, 5);
            let png = encode_png(&img).expect("encode");
            let back = decode_png(&png).expect("decode");
            assert_eq!((back.width, back.height), (7, 5));
            assert_eq!(back.bytes.as_ref(), img.bytes.as_ref(), "RGBA must survive PNG round-trip");
        }

        #[test]
        fn image_hash_survives_png_reencode() {
            // The echo-suppression invariant: a write() stores the hash of the
            // RGBA it set; the poller later reads that RGBA back and must compute
            // the *same* hash so it doesn't re-offer our own write. PNG bytes are
            // not byte-stable across encode, so the hash is over RGBA, not PNG.
            let img = sample(16, 16);
            let written = image_hash(img.width, img.height, &img.bytes);

            let png = encode_png(&img).expect("encode");
            let reread = decode_png(&png).expect("decode");
            let seen = image_hash(reread.width, reread.height, &reread.bytes);

            assert_eq!(written, seen, "image echo hash must be stable across PNG round-trip");
        }

        #[test]
        fn decode_rejects_garbage() {
            assert!(decode_png(b"not a png").is_none());
        }
    }
}
