//! Daemon process management for the tray UI.
//!
//! The GUI can launch and stop `deskorynd` itself (so the user never drops to a
//! terminal), in two roles:
//!
//! * **run** — the long-lived daemon. Plain `run` is the symmetric "server/auto"
//!   role (listen + mDNS discovery + remembered peers); `run --connect <addr>`
//!   is the "client" role that also proactively dials a known address.
//! * **pair** — first-contact pairing. `pair --listen` waits (server); `pair
//!   <addr>` dials (client). We pipe its stdout to extract the 6-digit SAS and
//!   write the user's confirmation back to its stdin.
//!
//! The `deskorynd` binary is auto-resolved (env override → next to this UI
//! binary → dev `target/{debug,release}` → `PATH`), with a user override that
//! persists in the UI's config dir.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

const BIN: &str = if cfg!(windows) { "deskorynd.exe" } else { "deskorynd" };

/// Tauri-managed state: the child processes we own plus the binary override.
#[derive(Default)]
pub struct ProcMgr {
    run: Mutex<Option<Child>>,
    pair: Mutex<Option<PairProc>>,
    bin_override: Mutex<Option<PathBuf>>,
}

struct PairProc {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Serialize)]
pub struct BinInfo {
    /// Resolved absolute path, if one was found.
    path: Option<String>,
    /// Where it came from: "override" | "sibling" | "dev" | "path" | "none".
    source: String,
    exists: bool,
}

#[derive(Serialize)]
pub struct Lifecycle {
    /// Whether the `run` daemon we spawned is still alive.
    running: bool,
    /// Whether a pairing subprocess is in flight.
    pairing: bool,
}

#[derive(Deserialize, Default)]
struct UiSettings {
    daemon_bin: Option<String>,
}

fn settings_file() -> Option<PathBuf> {
    directories::ProjectDirs::from("ch", "biceps", "deskoryn")
        .map(|p| p.config_dir().join("ui.json"))
}

/// Load the persisted binary override into state (called once at startup).
pub async fn load_override(mgr: &ProcMgr) {
    if let Some(f) = settings_file() {
        if let Ok(bytes) = std::fs::read(&f) {
            if let Ok(s) = serde_json::from_slice::<UiSettings>(&bytes) {
                if let Some(p) = s.daemon_bin {
                    *mgr.bin_override.lock().await = Some(PathBuf::from(p));
                }
            }
        }
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
    // Next to the UI binary (the installed layout).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(BIN);
            if cand.is_file() {
                return (Some(cand), "sibling");
            }
            // Dev layout: ui/src-tauri/target/debug → repo target/{debug,release}.
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
    // PATH lookup.
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

#[tauri::command]
pub async fn daemon_bin_info(state: State<'_, ProcMgr>) -> Result<BinInfo, String> {
    let ov = state.bin_override.lock().await.clone();
    let (path, source) = resolve(&ov);
    let exists = path.as_ref().map(|p| p.is_file()).unwrap_or(false);
    Ok(BinInfo {
        path: path.map(|p| p.display().to_string()),
        source: source.to_string(),
        exists,
    })
}

#[tauri::command]
pub async fn set_daemon_bin(
    state: State<'_, ProcMgr>,
    path: Option<String>,
) -> Result<BinInfo, String> {
    let p = path.filter(|s| !s.trim().is_empty()).map(PathBuf::from);
    {
        let mut ov = state.bin_override.lock().await;
        *ov = p.clone();
        persist_override(&ov);
    }
    daemon_bin_info(state).await
}

#[tauri::command]
pub async fn daemon_lifecycle(state: State<'_, ProcMgr>) -> Result<Lifecycle, String> {
    let mut run = state.run.lock().await;
    let running = match run.as_mut() {
        // try_wait returns Ok(Some(_)) once the child has exited.
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    };
    if !running {
        *run = None;
    }
    let pairing = state.pair.lock().await.is_some();
    Ok(Lifecycle { running, pairing })
}

async fn bin_or_err(state: &State<'_, ProcMgr>) -> Result<PathBuf, String> {
    let ov = state.bin_override.lock().await.clone();
    match resolve(&ov).0 {
        Some(p) if p.is_file() => Ok(p),
        Some(p) => Err(format!("deskorynd not found at {}", p.display())),
        None => Err("deskorynd binary not found (set its path in Settings)".into()),
    }
}

/// Stream one reader's lines to the frontend as `daemon-log` events.
fn pump_reader<R>(app: AppHandle, reader: R, tag: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = app.emit("daemon-log", format!("[{tag}] {line}"));
        }
    });
}

/// Stream a child's stdout and stderr to the frontend as `daemon-log` events.
fn pump_logs(app: AppHandle, child: &mut Child, tag: &'static str) {
    if let Some(out) = child.stdout.take() {
        pump_reader(app.clone(), out, tag);
    }
    if let Some(err) = child.stderr.take() {
        pump_reader(app, err, tag);
    }
}

#[tauri::command]
pub async fn daemon_start(
    app: AppHandle,
    state: State<'_, ProcMgr>,
    connect: Option<String>,
) -> Result<(), String> {
    {
        // Refuse to double-start.
        let mut run = state.run.lock().await;
        if let Some(child) = run.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return Err("daemon already running".into());
            }
        }
    }
    let bin = bin_or_err(&state).await?;
    let mut cmd = Command::new(&bin);
    cmd.arg("run");
    if let Some(addr) = connect.as_ref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--connect").arg(addr.trim());
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false);
    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    pump_logs(app, &mut child, "run");
    *state.run.lock().await = Some(child);
    Ok(())
}

#[tauri::command]
pub async fn daemon_stop(state: State<'_, ProcMgr>) -> Result<(), String> {
    if let Some(mut child) = state.run.lock().await.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    Ok(())
}

#[tauri::command]
pub async fn pair_start(
    app: AppHandle,
    state: State<'_, ProcMgr>,
    listen: bool,
    addr: Option<String>,
) -> Result<(), String> {
    if state.pair.lock().await.is_some() {
        return Err("a pairing is already in progress".into());
    }
    // Pairing binds the listen port; it can't share it with a running daemon.
    if let Some(child) = state.run.lock().await.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            return Err("stop the daemon before pairing (they share the port)".into());
        }
    }
    let bin = bin_or_err(&state).await?;
    let mut cmd = Command::new(&bin);
    cmd.arg("pair");
    if listen {
        cmd.arg("--listen");
    } else {
        let a = addr
            .filter(|s| !s.trim().is_empty())
            .ok_or("enter the peer's host:port to pair as client")?;
        cmd.arg(a.trim());
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", bin.display()))?;

    let stdin = child.stdin.take().ok_or("no stdin on pair process")?;
    // stderr is logged; stdout is consumed by the SAS parser below (which also
    // forwards each line to the log).
    if let Some(err) = child.stderr.take() {
        pump_reader(app.clone(), err, "pair");
    }

    // Parse the pair process's stdout for the SAS and the outcome, surfacing
    // them to the frontend as events. `pair` writes the code as `NNN NNN` and
    // the peer name on a `  Pair with "name" (...)` line.
    if let Some(out) = child.stdout.take() {
        let app = app.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            let mut peer = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = app.emit("daemon-log", format!("[pair] {line}"));
                let t = line.trim();
                if let Some(name) = parse_peer_name(t) {
                    peer = name;
                }
                if let Some(sas) = parse_sas(t) {
                    let _ = app.emit(
                        "pair-sas",
                        serde_json::json!({ "sas": sas, "device_name": peer }),
                    );
                }
                if t.starts_with("Paired with") {
                    let _ = app.emit("pair-result", serde_json::json!({ "ok": true }));
                } else if t.contains("aborted") {
                    let _ = app.emit("pair-result", serde_json::json!({ "ok": false }));
                }
            }
            let _ = app.emit("pair-ended", ());
        });
    }

    *state.pair.lock().await = Some(PairProc { child, stdin });
    Ok(())
}

/// Answer the pair process's `[y/N]` prompt.
#[tauri::command]
pub async fn pair_respond(state: State<'_, ProcMgr>, accept: bool) -> Result<(), String> {
    let mut guard = state.pair.lock().await;
    let p = guard.as_mut().ok_or("no pairing in progress")?;
    let line = if accept { "y\n" } else { "n\n" };
    p.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    p.stdin.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn pair_cancel(state: State<'_, ProcMgr>) -> Result<(), String> {
    if let Some(mut p) = state.pair.lock().await.take() {
        let _ = p.child.start_kill();
        let _ = p.child.wait().await;
    }
    Ok(())
}

/// Clear a finished pairing process from state (called when `pair-ended` fires).
#[tauri::command]
pub async fn pair_reap(state: State<'_, ProcMgr>) -> Result<(), String> {
    *state.pair.lock().await = None;
    Ok(())
}

fn parse_sas(line: &str) -> Option<String> {
    // Exactly `NNN NNN`.
    let bytes = line.as_bytes();
    if bytes.len() == 7
        && bytes[3] == b' '
        && bytes[..3].iter().all(u8::is_ascii_digit)
        && bytes[4..].iter().all(u8::is_ascii_digit)
    {
        Some(line.to_string())
    } else {
        None
    }
}

fn parse_peer_name(line: &str) -> Option<String> {
    // `Pair with "name" (id)`
    let rest = line.strip_prefix("Pair with \"")?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
