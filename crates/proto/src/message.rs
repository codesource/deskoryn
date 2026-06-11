//! Message types for every channel.

use deskoryn_core::config::AudioProfile;
use deskoryn_core::input::InputEvent;
use deskoryn_core::layout::VirtualDesktop;
use deskoryn_core::DeviceId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

/// A monotonically increasing identifier for a single file transfer or a single
/// clipboard pull, scoped to a session.
pub type StreamTag = u64;

// ---------------------------------------------------------------------------
// Control channel
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Control {
    /// First message after the secure channel is up. Exchanged both ways.
    Hello {
        version: ProtocolVersion,
        device: DeviceId,
        name: String,
        /// Monitors this device contributes, already placed in virtual space.
        monitors: VirtualDesktop,
        capabilities: Capabilities,
    },
    /// Liveness probe; `Pong` echoes the nonce. Drives reconnect detection.
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    /// Either side proposes an updated full virtual-desktop layout (e.g. a
    /// monitor was plugged in, or the user rearranged the grid in the UI).
    LayoutUpdate { layout: VirtualDesktop },
    /// Graceful shutdown notice so the peer can release the cursor immediately
    /// instead of waiting for a timeout.
    Goodbye { reason: String },
    /// Out-of-band error report (non-fatal).
    Error { code: ErrorCode, detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub clipboard_text: bool,
    pub clipboard_images: bool,
    pub clipboard_files: bool,
    pub file_transfer: bool,
    pub audio_forward: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Protocol,
    Unauthorized,
    Busy,
    Internal,
}

// ---------------------------------------------------------------------------
// Input channel
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    /// Control of the cursor is handed to this peer; it should warp the local
    /// pointer to `entry` and begin injecting events locally.
    Enter {
        entry: deskoryn_core::geometry::Point,
        /// Sender's current modifier state so the receiver starts consistent.
        mods: deskoryn_core::input::Modifiers,
    },
    /// Control leaves this peer (cursor returned to the other machine).
    Leave,
    /// A batch of input events to inject. Batching amortizes framing overhead
    /// while keeping latency low (flushed every event or every few ms).
    Events { seq: u32, events: Vec<InputEvent> },
    /// Acknowledgement of the highest `seq` processed (for loss/latency stats).
    Ack { seq: u32 },
}

// ---------------------------------------------------------------------------
// Clipboard channel
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Clipboard {
    /// "My clipboard changed; here is what formats I can provide." Small text
    /// may be inlined; everything else is pulled on demand by `Pull`.
    Offer {
        seq: u64,
        formats: Vec<ClipFormat>,
        /// Present only for small payloads under the inline threshold.
        inline: Option<ClipPayload>,
    },
    /// "Send me this format from your latest offer (seq)."
    Pull { seq: u64, format: ClipFormat, tag: StreamTag },
    /// Inline reply to `Pull` for payloads still small enough to inline.
    Data { tag: StreamTag, payload: ClipPayload },
    /// The payload will arrive on a dedicated FileXfer-style stream tagged `tag`.
    DataStream { tag: StreamTag, format: ClipFormat, len: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipFormat {
    Utf8Text,
    Html,
    Png,
    /// A list of files/folders (RDP-style file-group clipboard). The actual
    /// bytes move via the file-transfer machinery.
    FileList,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClipPayload {
    Text(String),
    Html(String),
    Bytes(Vec<u8>),
    /// File-list clipboard: metadata only; bytes are fetched per file.
    Files(Vec<super::FileEntry>),
}

// ---------------------------------------------------------------------------
// File-transfer channel
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FileXfer {
    /// Propose a transfer of one or more files/folders.
    Offer { tag: StreamTag, manifest: Manifest },
    /// Accept (optionally resuming each file from an offset) or reject.
    Accept { tag: StreamTag, resume: Vec<FileResume> },
    Reject { tag: StreamTag, reason: String },
    /// Sender's progress heartbeat (also derivable from bytes received, but this
    /// lets the receiver show progress before the first chunk of a huge file).
    Progress { tag: StreamTag, file_index: u32, bytes_done: u64 },
    /// All files delivered.
    Complete { tag: StreamTag },
    /// Abort an in-flight transfer.
    Cancel { tag: StreamTag, reason: String },
}

/// Description of a transfer set. Bytes are streamed separately (see
/// `docs/PROTOCOL.md` — the chunk stream is framed `[file_index][offset][len][bytes]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the transfer root (forward-slashed, normalized).
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    /// Unix mtime seconds, preserved across platforms where possible.
    pub mtime: Option<u64>,
    /// Unix permission bits if the source is POSIX (advisory on Windows).
    pub mode: Option<u32>,
    /// BLAKE3 of the contents, used for resume validation and dedupe.
    pub hash: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FileResume {
    pub file_index: u32,
    pub offset: u64,
}

// ---------------------------------------------------------------------------
// Audio channel (carried over QUIC datagrams)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioControl {
    /// Begin a stream; subsequent datagrams carry [`AudioFrame`]s with this tag.
    Start {
        tag: StreamTag,
        profile: AudioProfile,
        sample_rate: u32,
        channels: u8,
        /// Opus frame duration in microseconds (e.g. 2500/5000/10000).
        frame_us: u32,
    },
    Stop { tag: StreamTag },
}

/// One Opus packet. Sent as a QUIC datagram; loss is tolerated (the decoder
/// applies packet-loss concealment).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioFrame {
    pub tag: StreamTag,
    /// Sample-clock sequence number for ordering and concealment.
    pub seq: u32,
    pub opus: Vec<u8>,
}
