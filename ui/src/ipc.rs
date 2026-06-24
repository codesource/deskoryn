//! Thin client for `deskorynd`'s local control channel.
//!
//! Mirrors the daemon's protocol (`crates/daemon/src/ipc.rs`): length-prefixed
//! JSON over a Unix domain socket (Unix/macOS) or a named pipe (Windows). Keep
//! the field names and `serde` tags in lockstep with the daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiRequest {
    Status,
    /// Empty `addr` = make discoverable (wait); non-empty = dial that peer.
    Pair { addr: String },
    PairConfirm { accept: bool },
    PairStatus,
    PairCancel,
    DiscoveredPeers,
    Forget { device: String },
    SetLayout { layout: serde_json::Value },
    SetFeature { feature: Feature, enabled: bool },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    ClipboardSync,
    AudioForward,
    InputSharing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    Status {
        device_name: String,
        peers: Vec<PeerStatus>,
        active: bool,
        #[serde(default)]
        port: u16,
    },
    Pairing {
        phase: String,
        sas: String,
        peer: String,
    },
    Discovered {
        peers: Vec<DiscoveredPeer>,
    },
    TransferProgress {
        tag: u64,
        name: String,
        fraction: f32,
        bytes_per_sec: u64,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    pub name: String,
    pub addr: String,
    pub device: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub connected: bool,
    pub address: Option<String>,
    pub latency_ms: Option<u32>,
}

/// Resolve the daemon's control-socket path. Must match
/// `deskoryn_core::config::Paths` exactly: `ProjectDirs::from("io","Deskoryn",
/// "Deskoryn").data_dir()/deskorynd.sock`. (The daemon does **not** honor a
/// state-dir env override, so neither do we.)
pub fn socket_path() -> PathBuf {
    directories::ProjectDirs::from("io", "Deskoryn", "Deskoryn")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("Deskoryn"))
        .join("deskorynd.sock")
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn write_msg<T: Serialize, W: AsyncWriteExt + Unpin>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

async fn read_msg<T: serde::de::DeserializeOwned, R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control-channel frame too large",
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    let msg = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

/// Send one request and collect the daemon's response events. Returns a
/// human-readable error string (so it can ride an iced `Message`, which is `Clone`).
pub async fn request(req: UiRequest) -> Result<Vec<UiEvent>, String> {
    let path = socket_path();

    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|e| format!("daemon not reachable ({e})"))?;
        write_msg(&mut stream, &req).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(ev) = read_msg::<UiEvent, _>(&mut stream).await.map_err(|e| e.to_string())? {
            out.push(ev);
        }
        Ok(out)
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let name = format!(
            r"\\.\pipe\{}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("deskorynd.sock")
        );
        // ERROR_PIPE_BUSY (231): all server instances are busy; the canonical
        // Windows behaviour is to wait briefly and retry (the daemon stands up a
        // fresh instance after each accept).
        const ERROR_PIPE_BUSY: i32 = 231;
        let mut client = {
            let mut tries = 0;
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(c) => break c,
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                        tries += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => return Err(format!("daemon not reachable ({e})")),
                }
            }
        };
        write_msg(&mut client, &req).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(ev) = read_msg::<UiEvent, _>(&mut client).await.map_err(|e| e.to_string())? {
            out.push(ev);
        }
        Ok(out)
    }
}
