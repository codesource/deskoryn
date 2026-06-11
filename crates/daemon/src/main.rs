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

mod focus;
mod ipc;
mod session;
mod supervisor;

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
    /// Begin pairing with a peer at host:port.
    Pair { addr: String },
    /// List remembered (trusted) devices.
    Devices,
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
        Cmd::Pair { addr } => {
            // TODO(impl): dial, run the SAS flow (deskoryn_net::pairing), prompt
            // the user to compare codes, then persist the trust record.
            println!("Pairing with {addr} — compare the 6-digit code on both screens.");
            Ok(())
        }
        Cmd::Devices => {
            let store = deskoryn_core::trust::TrustStore::load(&paths.trust_file())?;
            for d in &store.devices {
                println!("{}  {}  {}", d.id.short(), d.name, d.fingerprint.short());
            }
            Ok(())
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "deskoryn-device".into())
}
