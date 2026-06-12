//! Top-level supervision: bring up endpoint + discovery, then spawn and restart
//! a [`session`](crate::session) task per connected peer. Implements the
//! "reconnect after sleep / reboot / network blip" requirement by treating a
//! dropped session as normal and re-dialing with backoff.

use deskoryn_core::config::{AppConfig, Paths};
use std::sync::Arc;
#[cfg(any(feature = "linux", feature = "windows"))]
use std::time::Duration;

pub async fn run(config: Arc<AppConfig>, paths: Paths, dry_run: bool) -> anyhow::Result<()> {
    tracing::info!(
        device = %config.device.name,
        id = %config.device.id,
        dry_run,
        "deskorynd starting"
    );

    if dry_run {
        return run_dry(config).await;
    }

    #[cfg(any(feature = "linux", feature = "windows"))]
    {
        run_real(config, paths).await
    }

    #[cfg(not(any(feature = "linux", feature = "windows")))]
    {
        let _ = paths;
        tracing::warn!(
            "real transport not built in; rebuild with `--features linux` (or `windows`). Idling."
        );
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Real, networked run: bind a QUIC endpoint, accept inbound sessions, and dial
/// known peers with auto-reconnect. Requires the `linux`/`windows` feature
/// (which enables `deskoryn-net/full`).
#[cfg(any(feature = "linux", feature = "windows"))]
async fn run_real(config: Arc<AppConfig>, paths: Paths) -> anyhow::Result<()> {
    use deskoryn_core::trust::TrustStore;
    use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint};
    use tokio::sync::Mutex;

    let identity = Arc::new(DeviceIdentity::load_or_generate(
        config.device.id,
        &paths.cert_file(),
        &paths.key_file(),
    )?);
    tracing::info!(fingerprint = %identity.fingerprint.short(), "device identity ready");

    let identity_fp = identity.fingerprint;
    let trust = Arc::new(Mutex::new(TrustStore::load(&paths.trust_file())?));
    let endpoint = Arc::new(QuicEndpoint::bind(config.network.listen_port, identity, trust.clone()).await?);
    tracing::info!(port = endpoint.local_port(), "QUIC endpoint bound");

    // Local control socket for the tray UI / `deskorynd status`.
    #[cfg(unix)]
    {
        use crate::ipc::{self, PeerStatus, UiEvent, UiRequest};
        let device_name = config.device.name.clone();
        let peer_names: Vec<String> = trust.lock().await.devices.iter().map(|d| d.name.clone()).collect();
        let socket = paths.socket_file();
        let handler: ipc::Handler = std::sync::Arc::new(move |req| match req {
            UiRequest::Status => vec![UiEvent::Status {
                device_name: device_name.clone(),
                // TODO(impl): report live connection state from the session tasks.
                peers: peer_names
                    .iter()
                    .map(|n| PeerStatus { name: n.clone(), connected: false, address: None, latency_ms: None })
                    .collect(),
                active: false,
            }],
            _ => vec![],
        });
        tracing::info!(socket = %socket.display(), "control socket listening");
        let socket_for_serve = socket.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::serve(socket_for_serve, handler).await {
                tracing::warn!(error = %e, "control socket ended");
            }
        });
    }

    if config.network.discovery_enabled {
        match start_discovery(&config, &endpoint, &trust, identity_fp).await {
            Ok(()) => tracing::info!(name = %config.device.name, "advertising on mDNS"),
            Err(e) => tracing::warn!(error = %e, "mDNS discovery unavailable"),
        }
    }

    // Accept loop: every inbound, authenticated session gets its own task.
    {
        let endpoint = endpoint.clone();
        let config = config.clone();
        tokio::spawn(async move {
            loop {
                match endpoint.accept().await {
                    Ok(session) => {
                        let config = config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = crate::session::run(config, session).await {
                                tracing::warn!(error = %e, "inbound session ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "accept rejected/failed");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        });
    }

    // Dial loop: one reconnecting task per known address (static peers +
    // remembered `last_address`es).
    let mut addrs: Vec<String> = config.network.static_peers.clone();
    {
        let t = trust.lock().await;
        for d in &t.devices {
            if let Some(a) = &d.last_address {
                addrs.push(a.clone());
            }
        }
    }
    addrs.sort();
    addrs.dedup();

    for addr_str in addrs {
        let endpoint = endpoint.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let mut backoff = Backoff::default();
            loop {
                match addr_str.parse() {
                    Ok(addr) => match endpoint.connect_any(addr).await {
                        Ok(session) => {
                            backoff = Backoff::default();
                            if let Err(e) = crate::session::run(config.clone(), session).await {
                                tracing::warn!(error = %e, peer = %addr_str, "session ended");
                            }
                        }
                        Err(e) => tracing::debug!(error = %e, peer = %addr_str, "dial failed"),
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "invalid peer address: {addr_str}");
                        return;
                    }
                }
                tokio::time::sleep(backoff.next()).await;
            }
        });
    }

    // The spawned loops do the work; park until shutdown.
    std::future::pending::<()>().await;
    Ok(())
}

/// Start mDNS: advertise this device and dial discovered, **already-trusted**
/// peers. To avoid both peers dialing each other, only the device with the
/// lexicographically smaller id initiates; the other waits to be accepted.
#[cfg(any(feature = "linux", feature = "windows"))]
async fn start_discovery(
    config: &Arc<AppConfig>,
    endpoint: &Arc<deskoryn_net::quic::QuicEndpoint>,
    trust: &Arc<tokio::sync::Mutex<deskoryn_core::trust::TrustStore>>,
    fingerprint: deskoryn_core::trust::CertFingerprint,
) -> anyhow::Result<()> {
    use deskoryn_net::discovery::{mdns::MdnsDiscovery, Discovery};

    let discovery = Arc::new(MdnsDiscovery::new()?);
    discovery
        .advertise(config.device.id, &config.device.name, endpoint.local_port(), fingerprint)
        .await?;

    let endpoint = endpoint.clone();
    let config = config.clone();
    let trust = trust.clone();
    // Peers we already have a session with (or are dialing), so repeated mDNS
    // re-resolutions don't spawn a storm of duplicate connections.
    let active: Arc<tokio::sync::Mutex<std::collections::HashSet<deskoryn_core::DeviceId>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    tokio::spawn(async move {
        while let Some(hint) = discovery.next_peer().await {
            let trusted = trust.lock().await.get(hint.device).is_some();
            if !trusted {
                tracing::debug!(peer = %hint.device.short(), "discovered an unpaired peer; run `deskorynd pair` to connect");
                continue;
            }
            // Deterministic single-dialer rule: only the smaller id initiates.
            if config.device.id.as_bytes() >= hint.device.as_bytes() {
                continue;
            }
            // Skip if we're already connected to / dialing this peer.
            {
                let mut active = active.lock().await;
                if !active.insert(hint.device) {
                    continue;
                }
            }
            tracing::info!(peer = %hint.name, addr = %hint.addr, "discovered trusted peer; connecting");
            match endpoint.connect_any(hint.addr).await {
                Ok(session) => {
                    let config = config.clone();
                    let active = active.clone();
                    let device = hint.device;
                    tokio::spawn(async move {
                        if let Err(e) = crate::session::run(config, session).await {
                            tracing::warn!(error = %e, "discovered session ended");
                        }
                        active.lock().await.remove(&device);
                    });
                }
                Err(e) => {
                    active.lock().await.remove(&hint.device);
                    tracing::debug!(error = %e, "dial of discovered peer failed");
                }
            }
        }
    });
    Ok(())
}

/// Run the whole daemon in one process over a loopback session, with a synthetic
/// peer. Exercises the orchestration without touching the OS or network.
async fn run_dry(config: Arc<AppConfig>) -> anyhow::Result<()> {
    use deskoryn_net::transport::{loopback, Session};
    use deskoryn_proto::{Capabilities, PROTOCOL_VERSION};

    let me = config.device.id;
    let peer = deskoryn_core::DeviceId::from_bytes([0xEE; 16]);
    let (mine, theirs) = loopback::loopback(me, peer);

    // Pretend the peer is another daemon: reply to the Hello, answer a couple of
    // heartbeats, then say goodbye so the dry-run ends cleanly.
    let peer_task = tokio::spawn(async move {
        use deskoryn_proto::{decode_one, encode, Control};
        let mut buf = bytes::BytesMut::new();
        let (mut sink, mut src) = theirs.channel(deskoryn_proto::Channel::Control).await.unwrap();

        // Wait for our Hello, then reply with one.
        if let Ok(Some(frame)) = src.recv_bytes().await {
            tracing::info!(bytes = frame.len(), "synthetic peer received Hello");
            let reply = Control::Hello {
                version: PROTOCOL_VERSION,
                device: peer,
                name: "synthetic-peer".into(),
                monitors: Default::default(),
                capabilities: Capabilities {
                    clipboard_text: true,
                    clipboard_images: true,
                    clipboard_files: true,
                    file_transfer: true,
                    audio_forward: true,
                },
            };
            buf.clear();
            encode(&reply, &mut buf).unwrap();
            let _ = sink.send_bytes(&buf).await;
        }

        // Answer two pings, then bid goodbye.
        let mut pongs = 0;
        while let Ok(Some(frame)) = src.recv_bytes().await {
            let mut b = bytes::BytesMut::from(&frame[..]);
            if let Ok(Some(Control::Ping { nonce })) = decode_one::<Control>(&mut b) {
                buf.clear();
                encode(&Control::Pong { nonce }, &mut buf).unwrap();
                let _ = sink.send_bytes(&buf).await;
                tracing::info!(nonce, "synthetic peer ponged");
                pongs += 1;
                if pongs >= 2 {
                    buf.clear();
                    encode(&Control::Goodbye { reason: "demo complete".into() }, &mut buf).unwrap();
                    let _ = sink.send_bytes(&buf).await;
                    break;
                }
            }
        }
    });

    crate::session::run(config, Box::new(mine)).await?;
    let _ = peer_task.await;
    Ok(())
}

/// Capped exponential backoff for reconnection.
#[cfg(any(feature = "linux", feature = "windows"))]
struct Backoff {
    current: Duration,
    max: Duration,
}

#[cfg(any(feature = "linux", feature = "windows"))]
impl Default for Backoff {
    fn default() -> Self {
        Self {
            current: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

#[cfg(any(feature = "linux", feature = "windows"))]
impl Backoff {
    fn next(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }
}
