//! `deskorynd` — the Deskoryn background service.
//!
//! Responsibilities:
//! * load config + trust store, generate the device identity on first run;
//! * advertise/discover peers and bring up secure sessions (auto-reconnect);
//! * run the [`focus`] state machine that owns the shared cursor and decides
//!   when control crosses the machine boundary;
//! * fan messages out to / in from the feature modules (input, clipboard,
//!   files, audio) over the session channels;
//! * expose a small local control socket for the tray UI / CLI.
//!
//! Most module bodies here are orchestration skeletons with `TODO(impl)` markers;
//! the per-feature logic lives in the respective crates. The whole thing runs
//! end-to-end today over the in-memory loopback session (`--dry-run`).

mod arrange;
mod audio;
mod clipboard;
mod control;
mod diag;
mod focus;
mod input;
mod ipc;
mod pair;
mod session;
mod supervisor;
mod transfer;

use clap::{Parser, Subcommand};
use deskoryn_core::config::{AppConfig, Paths};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "deskorynd", version, about = "Deskoryn unified-workstation daemon")]
struct Cli {
    /// Override the config file path.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (default).
    Run {
        /// Run fully in-process over a loopback session; touches no real OS
        /// input/clipboard/audio. Useful for development and CI.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print resolved config + paths and exit.
    Info,
    /// Pair with a peer: dial `host:port`, or wait with `--listen`.
    Pair {
        /// Address of the peer to dial (omit when using --listen).
        addr: Option<String>,
        /// Wait for an incoming pairing request instead of dialing.
        #[arg(long)]
        listen: bool,
    },
    /// List remembered (trusted) devices.
    Devices,
    /// Send files to a paired peer at host:port.
    Send {
        /// Peer address (host:port).
        addr: String,
        /// Files or folders to send.
        #[arg(required = true)]
        files: Vec<std::path::PathBuf>,
    },
    /// Receive one incoming transfer from a paired peer.
    Receive {
        /// Destination directory (defaults to the configured download dir).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Query a running daemon over its local control socket.
    Status,
    /// Arrange this machine's monitors in the shared virtual desktop.
    Arrange {
        #[command(subcommand)]
        cmd: arrange::ArrangeCmd,
    },
    /// Exercise the local input backend in isolation (capture + optional inject).
    InputTest {
        /// Seconds to read input for.
        #[arg(long, default_value_t = 10)]
        secs: u64,
        /// Also inject a test cursor wiggle through the virtual device.
        #[arg(long)]
        inject: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,deskoryn=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let paths = Paths::resolve()?;
    let config_path = cli.config.unwrap_or_else(|| paths.config_file());
    let hostname = hostname();
    let config = Arc::new(AppConfig::load_or_bootstrap(&config_path, hostname)?);

    match cli.cmd.unwrap_or(Cmd::Run { dry_run: false }) {
        Cmd::Run { dry_run } => supervisor::run(config, paths, dry_run).await,
        Cmd::Info => {
            println!("device:     {} ({})", config.device.name, config.device.id);
            println!("config:     {}", config_path.display());
            println!("state dir:  {}", paths.state_dir.display());
            println!("monitors:   {}", config.layout.monitors.len());
            println!("discovery:  {}", config.network.discovery_enabled);
            Ok(())
        }
        Cmd::Pair { addr, listen } => pair::run(config, paths, addr, listen).await,
        Cmd::Send { addr, files } => transfer::send_command(config, paths, addr, files).await,
        Cmd::Receive { dir } => transfer::receive_command(config, paths, dir).await,
        Cmd::Status => status_command(&paths).await,
        Cmd::Arrange { cmd } => arrange::run(config, &config_path, cmd),
        Cmd::InputTest { secs, inject } => diag::input_test(secs, inject).await,
        Cmd::Devices => {
            let store = deskoryn_core::trust::TrustStore::load(&paths.trust_file())?;
            for d in &store.devices {
                println!("{}  {}  {}", d.id.short(), d.name, d.fingerprint.short());
            }
            Ok(())
        }
    }
}

/// Connect to a running daemon's control socket and print its status.
async fn status_command(paths: &Paths) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use ipc::{UiEvent, UiRequest};
        let socket = paths.socket_file();
        let events = ipc::request(&socket, &UiRequest::Status)
            .await
            .map_err(|e| anyhow::anyhow!("no running daemon at {} ({e})", socket.display()))?;
        for ev in events {
            if let UiEvent::Status { device_name, peers, active } = ev {
                println!("device: {device_name}");
                println!("active: {active}");
                if peers.is_empty() {
                    println!("peers:  (none paired)");
                } else {
                    println!("peers:");
                    for p in peers {
                        let state = if p.connected { "connected" } else { "offline" };
                        println!("  - {} [{state}]", p.name);
                    }
                }
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        anyhow::bail!("status over the control socket is currently Unix-only")
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "deskoryn-device".into())
}
