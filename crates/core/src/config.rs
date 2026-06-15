//! On-disk configuration (TOML) and the paths that hold runtime state.
//!
//! Configuration is human-editable TOML. Mutable security state (the trusted
//! device list, generated keypair/cert) lives separately under the state dir;
//! see [`crate::trust`] and `docs/CONFIG.md`.

use crate::ids::DeviceId;
use crate::layout::VirtualDesktop;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not determine platform config directory")]
    NoConfigDir,
}

/// Resolved, platform-correct locations for Deskoryn's files.
///
/// * Linux:   `~/.config/deskoryn`, `~/.local/share/deskoryn`
/// * Windows: `%APPDATA%\Deskoryn\config`, `%APPDATA%\Deskoryn\data`
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self, ConfigError> {
        let dirs = directories::ProjectDirs::from("io", "Deskoryn", "Deskoryn")
            .ok_or(ConfigError::NoConfigDir)?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            state_dir: dirs.data_dir().to_path_buf(),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn trust_file(&self) -> PathBuf {
        self.state_dir.join("trusted.json")
    }
    pub fn key_file(&self) -> PathBuf {
        self.state_dir.join("device.key")
    }
    pub fn cert_file(&self) -> PathBuf {
        self.state_dir.join("device.crt")
    }
    /// Local control socket (Unix domain socket / named pipe) for the tray/CLI.
    pub fn socket_file(&self) -> PathBuf {
        self.state_dir.join("deskorynd.sock")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub device: DeviceConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub file_transfer: FileTransferConfig,
    /// The saved virtual-desktop layout. Empty until the user arranges monitors
    /// or it is negotiated on first connect.
    #[serde(default)]
    pub layout: VirtualDesktop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: DeviceId,
    /// Friendly name shown to the peer (defaults to hostname on first run).
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// UDP/QUIC listen port (0 = OS-assigned, advertised over mDNS).
    pub listen_port: u16,
    /// Advertise & browse for peers via mDNS on the LAN.
    pub discovery_enabled: bool,
    /// Statically configured peers (host:port) for networks without mDNS.
    pub static_peers: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_port: 0,
            discovery_enabled: true,
            static_peers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputConfig {
    /// Keyboard focus follows the mouse across the machine boundary.
    pub focus_follows_mouse: bool,
    /// Pixels of "stickiness" at the desktop's outer edges before the cursor
    /// will leave a monitor (prevents accidental handoff). 0 disables.
    pub edge_resistance_px: i32,
    /// Hotkey (in the textual form parsed by `deskoryn-input`) that forces the
    /// cursor to the other machine regardless of position.
    pub switch_hotkey: String,
    /// Hotkey to lock the cursor to the current machine (disable transitions).
    pub lock_hotkey: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            focus_follows_mouse: true,
            edge_resistance_px: 0,
            switch_hotkey: "Ctrl+Alt+S".into(),
            lock_hotkey: "Ctrl+Alt+L".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardConfig {
    pub sync_text: bool,
    pub sync_images: bool,
    pub sync_files: bool,
    /// Inline payloads up to this size on the control stream; larger payloads
    /// are pulled on demand over a dedicated stream.
    pub inline_max_bytes: u64,
    /// How often the OS clipboard is polled for changes, in milliseconds. The
    /// cross-platform `arboard` backend exposes no change events, so the backend
    /// polls; lower = snappier paste, higher = less idle wakeups.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

fn default_poll_ms() -> u64 {
    250
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            sync_text: true,
            sync_images: true,
            sync_files: true,
            inline_max_bytes: 256 * 1024,
            poll_ms: 250,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioProfile {
    /// Opus at low bitrate, tiny frames, minimal jitter buffer (calls/gaming).
    LowLatency,
    /// Opus at high bitrate, larger jitter buffer (music/video).
    HighQuality,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioConfig {
    pub forward_enabled: bool,
    pub profile: AudioProfile,
    /// Capture device id on the source machine ("default" = system default).
    pub source_device: String,
    /// Playback device id on the destination machine.
    pub sink_device: String,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            forward_enabled: false,
            profile: AudioProfile::LowLatency,
            source_device: "default".into(),
            sink_device: "default".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Append " (2)", " (3)", ... to the incoming file name.
    Rename,
    Overwrite,
    Skip,
    /// Surface a prompt in the tray UI.
    Ask,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileTransferConfig {
    /// Where dropped/pasted files land by default.
    pub download_dir: Option<PathBuf>,
    pub conflict_policy: ConflictPolicy,
    /// Optional two-way synced folder pair (local path, peer path).
    pub shared_folders: Vec<SharedFolder>,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            download_dir: None,
            conflict_policy: ConflictPolicy::Rename,
            shared_folders: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedFolder {
    pub local_path: PathBuf,
    /// Logical name the peer maps to its own local path.
    pub name: String,
    pub bidirectional: bool,
}

impl AppConfig {
    /// A fresh config for a first run: random device id, hostname as name.
    pub fn bootstrap(name: impl Into<String>) -> Self {
        Self {
            device: DeviceConfig {
                id: DeviceId::generate(),
                name: name.into(),
            },
            network: NetworkConfig::default(),
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
            audio: AudioConfig::default(),
            file_transfer: FileTransferConfig::default(),
            layout: VirtualDesktop::default(),
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Load the config at `path`, or create and persist a bootstrap one.
    pub fn load_or_bootstrap(path: &std::path::Path, name: impl Into<String>) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            let cfg = Self::bootstrap(name);
            cfg.save(path)?;
            Ok(cfg)
        }
    }
}
