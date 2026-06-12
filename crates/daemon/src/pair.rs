//! `deskorynd pair` — interactive device pairing.
//!
//! Either dials a peer (`pair <addr>`) or waits for one (`pair --listen`),
//! exchanges identities, shows the 6-digit short-authentication string, and —
//! once the user confirms it matches on both screens — pins the peer's
//! certificate into the trust store. See `docs/SECURITY.md`.
//!
//! The real implementation needs the QUIC transport, so it is gated behind the
//! `linux`/`windows` features (which enable `deskoryn-net/full`).

use deskoryn_core::config::{AppConfig, Paths};
use std::sync::Arc;

/// Entry point dispatched from `main`. `addr` dials; `listen` waits to be dialed.
pub async fn run(
    config: Arc<AppConfig>,
    paths: Paths,
    addr: Option<String>,
    listen: bool,
) -> anyhow::Result<()> {
    #[cfg(any(feature = "linux", feature = "windows"))]
    {
        run_impl(config, paths, addr, listen).await
    }

    #[cfg(not(any(feature = "linux", feature = "windows")))]
    {
        let _ = (config, paths, addr, listen);
        anyhow::bail!("pairing requires a real build: rebuild with `--features linux` (or `windows`)")
    }
}

#[cfg(any(feature = "linux", feature = "windows"))]
async fn run_impl(
    config: Arc<AppConfig>,
    paths: Paths,
    addr: Option<String>,
    listen: bool,
) -> anyhow::Result<()> {
    use deskoryn_core::trust::TrustStore;
    use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint};
    use tokio::sync::Mutex;

    let identity = Arc::new(DeviceIdentity::load_or_generate(
        config.device.id,
        &paths.cert_file(),
        &paths.key_file(),
    )?);
    let trust = Arc::new(Mutex::new(TrustStore::load(&paths.trust_file())?));

    if listen {
        let endpoint = QuicEndpoint::bind(config.network.listen_port, identity.clone(), trust.clone()).await?;
        println!(
            "Waiting for a pairing request on port {} (this device: {} / {})",
            endpoint.local_port(),
            config.device.name,
            identity.fingerprint.short()
        );
        let session = endpoint.accept_unverified().await?;
        finish(&session, &config, identity.fingerprint, &trust, &paths, None).await
    } else {
        let addr_str = addr.ok_or_else(|| anyhow::anyhow!("provide host:port, or use --listen"))?;
        let socket = addr_str.parse().map_err(|e| anyhow::anyhow!("invalid address: {e}"))?;
        let endpoint = QuicEndpoint::bind(0, identity.clone(), trust.clone()).await?;
        println!("Connecting to {addr_str} …");
        let session = endpoint.connect_unverified(socket).await?;
        finish(&session, &config, identity.fingerprint, &trust, &paths, Some(addr_str)).await
    }
}

#[cfg(any(feature = "linux", feature = "windows"))]
async fn finish(
    session: &deskoryn_net::quic::QuicSession,
    config: &AppConfig,
    local_fp: deskoryn_core::trust::CertFingerprint,
    trust: &Arc<tokio::sync::Mutex<deskoryn_core::trust::TrustStore>>,
    paths: &Paths,
    last_address: Option<String>,
) -> anyhow::Result<()> {
    use deskoryn_net::pairing::PairingSession;

    let (peer_id, peer_name) = exchange(session, config.device.id, &config.device.name).await?;
    let cb = session.channel_binding()?;
    let pairing = PairingSession::new(
        config.device.id,
        local_fp,
        peer_id,
        session.fingerprint(),
        peer_name.clone(),
        &cb,
    );

    println!();
    println!("  Pair with \"{peer_name}\" ({})", peer_id.short());
    println!("  Confirm this code matches on BOTH screens:");
    println!();
    println!("        {}", pairing.sas.display());
    println!();
    println!("  If the codes differ, someone may be intercepting — do not continue.");

    if !prompt_yes("  Do the codes match? [y/N] ").await {
        println!("  Pairing aborted.");
        return Ok(());
    }

    {
        let mut t = trust.lock().await;
        t.upsert(pairing.confirm(now_unix(), last_address));
        t.save(&paths.trust_file())?;
    }
    println!("  Paired with {peer_name} ✓");
    Ok(())
}

/// Exchange `Control::Hello` over the Control channel; return the peer's id+name.
#[cfg(any(feature = "linux", feature = "windows"))]
async fn exchange(
    session: &deskoryn_net::quic::QuicSession,
    local: deskoryn_core::DeviceId,
    name: &str,
) -> anyhow::Result<(deskoryn_core::DeviceId, String)> {
    use bytes::BytesMut;
    use deskoryn_net::transport::Session;
    use deskoryn_proto::{decode_one, encode, Channel, Control, PROTOCOL_VERSION};

    let (mut sink, mut source) = session.channel(Channel::Control).await?;
    let hello = Control::Hello {
        version: PROTOCOL_VERSION,
        device: local,
        name: name.into(),
        monitors: Default::default(),
        capabilities: deskoryn_proto::Capabilities {
            clipboard_text: true,
            clipboard_images: true,
            clipboard_files: true,
            file_transfer: true,
            audio_forward: false,
        },
    };
    let mut buf = BytesMut::new();
    encode(&hello, &mut buf)?;
    sink.send_bytes(&buf).await?;

    let frame = source
        .recv_bytes()
        .await?
        .ok_or_else(|| anyhow::anyhow!("peer closed before sending its identity"))?;
    let mut b = BytesMut::from(&frame[..]);
    match decode_one::<Control>(&mut b)?.ok_or_else(|| anyhow::anyhow!("short frame"))? {
        Control::Hello { device, name, .. } => Ok((device, name)),
        other => Err(anyhow::anyhow!("expected Hello, got {other:?}")),
    }
}

/// Read a yes/no answer from stdin without blocking the async runtime.
#[cfg(any(feature = "linux", feature = "windows"))]
async fn prompt_yes(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    })
    .await
    .unwrap_or(false)
}

#[cfg(any(feature = "linux", feature = "windows"))]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
