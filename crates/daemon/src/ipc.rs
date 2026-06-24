//! Local control channel between the daemon and the tray UI / CLI.
//!
//! The tray app and `deskorynctl` are thin clients; all real work lives in the
//! daemon. They talk over a local-only endpoint (Unix domain socket on Linux,
//! named pipe on Windows) using a small JSON request/response protocol.
//!
//! This module defines the message vocabulary. The transport is a thin wrapper
//! the daemon binds and the UI connects to (TODO(impl)).

// The IPC vocabulary is defined ahead of the transport that uses it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Commands the UI sends to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiRequest {
    /// Current status snapshot (peers, active role, transfer list).
    Status,
    /// Start pairing. Empty `addr` opens the *discoverable* window (wait for a
    /// peer); a non-empty `addr` dials that peer to pair. Runs on the live
    /// daemon endpoint — no need to stop the daemon.
    Pair { addr: String },
    /// Confirm/deny the SAS comparison for an in-progress pairing.
    PairConfirm { accept: bool },
    /// Poll the current pairing flow (the channel is request/response).
    PairStatus,
    /// Cancel pairing / close the discoverable window / dismiss the result.
    PairCancel,
    /// List nearby devices currently advertising that they accept pairing.
    DiscoveredPeers,
    /// Forget a trusted device.
    Forget { device: String },
    /// Push an edited virtual-desktop layout.
    SetLayout { layout: deskoryn_core::VirtualDesktop },
    /// Toggle a feature at runtime.
    SetFeature { feature: Feature, enabled: bool },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    ClipboardSync,
    AudioForward,
    InputSharing,
}

/// Events/responses the daemon sends to the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    Status {
        device_name: String,
        peers: Vec<PeerStatus>,
        active: bool,
        /// The daemon's actual bound QUIC listen port (0 if not yet bound).
        #[serde(default)]
        port: u16,
    },
    /// Current pairing flow. `phase` ∈ idle | discoverable | connecting |
    /// prompt | paired | aborted | error; `sas`/`peer` set during prompt/result.
    Pairing { phase: String, sas: String, peer: String },
    /// Nearby devices accepting pairing (from mDNS), for the "waiting to pair" list.
    Discovered { peers: Vec<DiscoveredPeer> },
    /// File-transfer progress for the tray's progress UI.
    TransferProgress {
        tag: u64,
        name: String,
        fraction: f32,
        bytes_per_sec: u64,
    },
    /// Free-form notification (connection lost/restored, errors).
    Notice { level: NoticeLevel, text: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// A device discovered on the LAN that is advertising it accepts pairing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    pub name: String,
    /// `host:port` the UI passes back to `Pair { addr }` to connect.
    pub addr: String,
    /// Short device id (for display / disambiguation).
    pub device: String,
    /// Already in our trust store (so the UI can hide / mark it).
    pub trusted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub connected: bool,
    pub address: Option<String>,
    pub latency_ms: Option<u32>,
}

// ---------------------------------------------------------------------------
// Transport: length-prefixed JSON over a Unix domain socket (Unix/macOS) or a
// named pipe (Windows). The daemon serves; the tray UI / `deskorynctl` connect.
// The same path is passed on every OS; on Windows it maps to a pipe name.
// ---------------------------------------------------------------------------

#[cfg(any(unix, windows))]
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Windows named-pipe name derived from the control-socket path's file name,
/// e.g. `<state>/deskorynd.sock` → `\\.\pipe\deskorynd.sock`. The tray UI's
/// client derives the same name from its resolved socket path, so they meet.
#[cfg(windows)]
fn pipe_name(path: &Path) -> String {
    format!(
        r"\\.\pipe\{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("deskorynd.sock")
    )
}

/// A request handler: maps one [`UiRequest`] to the events to send back. Async
/// so handlers can touch the trust store / config (e.g. `Forget`) and report
/// live state, not just a startup snapshot.
pub type HandlerFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Vec<UiEvent>> + Send>>;
pub type Handler = std::sync::Arc<dyn Fn(UiRequest) -> HandlerFuture + Send + Sync>;

async fn write_msg<T: Serialize, W: AsyncWriteExt + Unpin>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

async fn read_msg<T: serde::de::DeserializeOwned, R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    let msg = serde_json::from_slice(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

/// Serve the control socket until cancelled. Each connection sends one request
/// and receives the handler's response events.
#[cfg(unix)]
pub async fn serve(path: PathBuf, handler: Handler) -> std::io::Result<()> {
    use tokio::net::UnixListener;
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;
    // Restrict to the owner.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    loop {
        let (mut stream, _) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Ok(Some(req)) = read_msg::<UiRequest, _>(&mut stream).await {
                for ev in handler(req).await {
                    if write_msg(&mut stream, &ev).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

/// Connect to the control socket, send one request, and collect the responses.
#[cfg(unix)]
pub async fn request(path: &Path, req: &UiRequest) -> std::io::Result<Vec<UiEvent>> {
    use tokio::net::UnixStream;
    let mut stream = UnixStream::connect(path).await?;
    write_msg(&mut stream, req).await?;
    let mut out = Vec::new();
    while let Some(ev) = read_msg::<UiEvent, _>(&mut stream).await? {
        out.push(ev);
    }
    Ok(out)
}

/// Serve the control channel over a Windows named pipe. Each accepted client
/// sends one request and receives the handler's response events; a fresh pipe
/// instance is created before handling so clients are served concurrently.
#[cfg(windows)]
pub async fn serve(path: PathBuf, handler: Handler) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = pipe_name(&path);
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)?;
    loop {
        // Wait for a client, then immediately stand up the next instance.
        server.connect().await?;
        let mut connected = server;
        server = ServerOptions::new().create(&name)?;

        let handler = handler.clone();
        tokio::spawn(async move {
            if let Ok(Some(req)) = read_msg::<UiRequest, _>(&mut connected).await {
                for ev in handler(req).await {
                    if write_msg(&mut connected, &ev).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

/// Connect to the daemon's named pipe, send one request, collect the responses.
#[cfg(windows)]
pub async fn request(path: &Path, req: &UiRequest) -> std::io::Result<Vec<UiEvent>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = pipe_name(path);
    // Retry on ERROR_PIPE_BUSY (231): all instances busy, wait and retry.
    let mut client = {
        let mut tries = 0;
        loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(231) && tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    };
    write_msg(&mut client, req).await?;
    let mut out = Vec::new();
    while let Some(ev) = read_msg::<UiEvent, _>(&mut client).await? {
        out.push(ev);
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ipc_status_round_trip() {
        let dir = std::env::temp_dir().join(format!("deskoryn-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("d.sock");

        let handler: Handler = std::sync::Arc::new(|req| {
            Box::pin(async move {
                match req {
                    UiRequest::Status => vec![UiEvent::Status {
                        device_name: "test-device".into(),
                        peers: vec![PeerStatus { name: "peer".into(), connected: true, address: None, latency_ms: Some(7) }],
                        active: true,
                        port: 7345,
                    }],
                    _ => vec![],
                }
            }) as HandlerFuture
        });
        let server = tokio::spawn(serve(sock.clone(), handler));

        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let resp = request(&sock, &UiRequest::Status).await.unwrap();
        assert_eq!(resp.len(), 1);
        match &resp[0] {
            UiEvent::Status { device_name, peers, active, port } => {
                assert_eq!(device_name, "test-device");
                assert!(*active);
                assert_eq!(*port, 7345);
                assert_eq!(peers[0].latency_ms, Some(7));
            }
            other => panic!("expected Status, got {other:?}"),
        }

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
