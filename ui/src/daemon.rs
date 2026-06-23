//! Daemon process management: the GUI launches and stops `deskorynd` itself, so
//! the user never needs a terminal. The `deskorynd` binary is auto-resolved
//! (env override → next to this UI binary → dev `target/{debug,release}` →
//! `PATH`), with a user override persisted in the UI config dir.
//!
//! Held behind an `Arc` in the iced app state; the async methods take
//! `Arc<Self>` so they can run inside `Task::perform`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const BIN: &str = if cfg!(windows) { "deskorynd.exe" } else { "deskorynd" };

#[derive(Default)]
pub struct ProcMgr {
    run: Mutex<Option<Child>>,
    bin_override: Mutex<Option<PathBuf>>,
}

#[derive(Clone, Debug)]
pub struct BinInfo {
    pub path: Option<String>,
    /// "override" | "sibling" | "dev" | "path" | "none"
    pub source: &'static str,
    pub exists: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Lifecycle {
    pub running: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct UiSettings {
    daemon_bin: Option<String>,
}

fn settings_file() -> Option<PathBuf> {
    directories::ProjectDirs::from("ch", "biceps", "deskoryn")
        .map(|p| p.config_dir().join("ui.json"))
}

impl ProcMgr {
    pub async fn load_override(&self) {
        if let Some(f) = settings_file() {
            if let Ok(bytes) = std::fs::read(&f) {
                if let Ok(s) = serde_json::from_slice::<UiSettings>(&bytes) {
                    if let Some(p) = s.daemon_bin {
                        *self.bin_override.lock().await = Some(PathBuf::from(p));
                    }
                }
            }
        }
    }

    pub async fn bin_info(self: Arc<Self>) -> BinInfo {
        let ov = self.bin_override.lock().await.clone();
        let (path, source) = resolve(&ov);
        let exists = path.as_ref().map(|p| p.is_file()).unwrap_or(false);
        BinInfo {
            path: path.map(|p| p.display().to_string()),
            source,
            exists,
        }
    }

    pub async fn set_bin(self: Arc<Self>, path: Option<String>) -> BinInfo {
        let p = path.filter(|s| !s.trim().is_empty()).map(PathBuf::from);
        {
            let mut ov = self.bin_override.lock().await;
            *ov = p.clone();
            persist_override(&ov);
        }
        self.bin_info().await
    }

    pub async fn lifecycle(self: Arc<Self>) -> Lifecycle {
        let mut run = self.run.lock().await;
        let running = match run.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if !running {
            *run = None;
        }
        Lifecycle { running }
    }

    async fn resolved_bin(&self) -> Result<PathBuf, String> {
        let ov = self.bin_override.lock().await.clone();
        match resolve(&ov).0 {
            Some(p) if p.is_file() => Ok(p),
            Some(p) => Err(format!("deskorynd not found at {}", p.display())),
            None => Err("deskorynd binary not found (set its path in Settings)".into()),
        }
    }

    /// Start `deskorynd run [--connect <addr>]`. `connect` selects the client role.
    pub async fn start(self: Arc<Self>, connect: Option<String>) -> Result<(), String> {
        {
            let mut run = self.run.lock().await;
            if let Some(child) = run.as_mut() {
                if matches!(child.try_wait(), Ok(None)) {
                    return Err("daemon already running".into());
                }
            }
        }
        let bin = self.resolved_bin().await?;
        let mut cmd = Command::new(&bin);
        cmd.arg("run");
        if let Some(addr) = connect.as_ref().filter(|s| !s.trim().is_empty()) {
            cmd.arg("--connect").arg(addr.trim());
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        // Drain the pipes so the child never blocks on a full stdout buffer.
        drain(child.stdout.take());
        drain(child.stderr.take());
        *self.run.lock().await = Some(child);
        Ok(())
    }

    pub async fn stop(self: Arc<Self>) -> Result<(), String> {
        if let Some(mut child) = self.run.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }
}

/// Spawn a task that swallows a child pipe's lines (prevents stdout backpressure).
fn drain<R>(reader: Option<R>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    if let Some(r) = reader {
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
        });
    }
}

fn persist_override(path: &Option<PathBuf>) {
    if let Some(f) = settings_file() {
        if let Some(parent) = f.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let s = UiSettings {
            daemon_bin: path.as_ref().map(|p| p.display().to_string()),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&s) {
            let _ = std::fs::write(&f, bytes);
        }
    }
}

/// Resolve the `deskorynd` binary, honouring the override first.
fn resolve(override_path: &Option<PathBuf>) -> (Option<PathBuf>, &'static str) {
    if let Some(p) = override_path {
        return (Some(p.clone()), "override");
    }
    if let Some(p) = std::env::var_os("DESKORYN_BIN") {
        return (Some(PathBuf::from(p)), "override");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(BIN);
            if cand.is_file() {
                return (Some(cand), "sibling");
            }
            for prof in ["debug", "release"] {
                let mut d = dir.to_path_buf();
                for _ in 0..6 {
                    let cand = d.join("target").join(prof).join(BIN);
                    if cand.is_file() {
                        return (Some(cand), "dev");
                    }
                    if !d.pop() {
                        break;
                    }
                }
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(BIN);
            if cand.is_file() {
                return (Some(cand), "path");
            }
        }
    }
    (None, "none")
}
