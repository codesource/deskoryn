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
        return run_dry(config, paths.config_file()).await;
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
/// Best-effort: register an inbound-UDP firewall allow rule for this exe so
/// peers can dial us for pairing/sessions. Windows silently blocks inbound UDP
/// for new apps (no prompt for UDP), which breaks pairing *into* this machine.
/// Succeeds when run elevated; otherwise logs a clear hint (the proper fix is an
/// installer-time rule). No-op on other platforms.
#[cfg(windows)]
fn ensure_firewall_rule() {
    use std::process::Command;
    let Ok(exe) = std::env::current_exe() else { return };

    // Already allowed? Querying needs no admin (unlike add/delete) and avoids
    // churning the rule on every start. Key off the exit status, not the
    // message text, which is localized.
    let exists = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", "name=Deskoryn"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if exists {
        tracing::debug!("firewall: Deskoryn inbound rule already present");
        return;
    }

    // Missing — add it (needs admin). If the exe later moves, delete the old
    // rule so this re-adds for the new path.
    let program = format!("program={}", exe.display());
    let res = Command::new("netsh")
        .args([
            "advfirewall", "firewall", "add", "rule", "name=Deskoryn",
            "dir=in", "action=allow", "protocol=UDP", "profile=any", &program,
        ])
        .output();
    match res {
        Ok(o) if o.status.success() => {
            tracing::info!("firewall: inbound UDP allowed for deskorynd")
        }
        Ok(o) => tracing::warn!(
            detail = %String::from_utf8_lossy(&o.stderr).trim(),
            "firewall rule not added — run the daemon once as Administrator (or add an inbound-UDP allow rule for deskorynd.exe) so peers can pair *to* this machine"
        ),
        Err(e) => tracing::warn!(error = %e, "could not run netsh to register a firewall rule"),
    }
}

/// Proactive inbound-reachability check, run when the user opens the
/// discoverable window. Windows only and precise: is our `netsh` allow rule
/// present? (Linux can only tell whether a firewall is *active*, not whether the
/// port is blocked — too false-positive-prone to warn on, so we rely on the
/// reactive dial-failure hint there instead.)
#[cfg(any(feature = "linux", feature = "windows"))]
fn inbound_firewall_hint() -> Option<String> {
    #[cfg(windows)]
    {
        let allowed = std::process::Command::new("netsh")
            .args(["advfirewall", "firewall", "show", "rule", "name=Deskoryn"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        return (!allowed).then(|| {
            "Windows is blocking inbound connections — allow deskorynd through the firewall \
             (run the daemon once as Administrator), or peers can't pair to this machine."
                .to_string()
        });
    }
    #[allow(unreachable_code)]
    None
}

/// This device's own dialable `ip:port` addresses, for the "connect by address"
/// hint shown while discoverable. Filters out un-routable addresses (loopback,
/// IPv4 link-local/APIPA `169.254/16`, IPv6 `fe80::/10` link-local) so we never
/// surface something a peer can't reach.
#[cfg(any(feature = "linux", feature = "windows"))]
fn local_addrs(port: u16) -> Vec<String> {
    use std::net::IpAddr;
    let dialable = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    };
    // Skip virtual/container/VPN interfaces — their addresses (Docker 172.x,
    // libvirt 192.168.122.x, WireGuard/Tailscale, etc.) aren't reachable from a
    // LAN peer. IP range alone can't tell these from a real LAN, so go by name.
    let is_virtual = |name: &str| {
        let n = name.to_ascii_lowercase();
        n == "lo"
            || [
                "docker", "br-", "virbr", "veth", "vnet", "vmnet", "vbox", "tun", "tap",
                "wg", "tailscale", "zt", "utun", "awdl", "llw",
            ]
            .iter()
            .any(|p| n.starts_with(p))
            || ["vethernet", "vmware", "virtualbox", "hyper-v", "loopback"]
                .iter()
                .any(|p| n.contains(p))
    };
    let mut v4: Vec<std::net::SocketAddr> = Vec::new();
    let mut v6: Vec<std::net::SocketAddr> = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if is_virtual(&iface.name) {
                continue;
            }
            let ip = iface.ip();
            if dialable(&ip) {
                let a = std::net::SocketAddr::new(ip, port);
                if ip.is_ipv4() { v4.push(a) } else { v6.push(a) }
            }
        }
    }
    // Sort numerically (SocketAddr/IpAddr order by value, not lexicographically).
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    v4.into_iter().chain(v6).map(|a| a.to_string()).collect() // IPv4 first
}

/// A peer last seen over mDNS.
#[cfg(any(feature = "linux", feature = "windows"))]
struct DiscEntry {
    name: String,
    /// All advertised dialable addresses (best first).
    addrs: Vec<std::net::SocketAddr>,
    pairing: bool,
    last_seen: std::time::Instant,
}

#[cfg(any(feature = "linux", feature = "windows"))]
type Discovered = tokio::sync::Mutex<std::collections::HashMap<deskoryn_core::DeviceId, DiscEntry>>;

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

    // Windows blocks inbound UDP for new apps without a prompt; best-effort
    // self-register a firewall rule so peers can pair *to* this machine.
    #[cfg(windows)]
    ensure_firewall_rule();

    // Pairing coordinator: shared by the IPC handler (open window / dial /
    // confirm) and the accept loop (route an untrusted peer into pairing).
    let pairing = Arc::new(crate::pairing::Pairing::default());

    // Live map of peers seen over mDNS, refreshed by the discovery loop and read
    // by the IPC handler for the "nearby waiting to pair" list.
    let discovered: Arc<Discovered> = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Live connection registry (which peers have a session right now, and their
    // monitors) and the saved per-peer arrangements, seeded from config.
    let registry: crate::session::ConnRegistry =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let layouts: crate::session::LayoutStore = Arc::new(Mutex::new(
        config.peer_layouts.iter().map(|p| (p.device, p.layout.clone())).collect(),
    ));

    // Local control channel for the tray UI / `deskorynd status` (Unix domain
    // socket on Unix/macOS, named pipe on Windows).
    #[cfg(any(unix, windows))]
    {
        use crate::ipc::{self, HandlerFuture, PeerStatus, UiEvent, UiRequest};
        let device_name = config.device.name.clone();
        let listen_port = endpoint.local_port();
        let socket = paths.socket_file();
        let trust_for_ipc = trust.clone();
        let trust_file = paths.trust_file();
        let pairing_ipc = pairing.clone();
        let endpoint_ipc = endpoint.clone();
        let config_ipc = config.clone();
        let paths_ipc = paths.clone();
        let local_fp_ipc = identity_fp;
        let discovered_ipc = discovered.clone();
        let registry_ipc = registry.clone();
        let layouts_ipc = layouts.clone();
        let device_id = config.device.id;

        // Map a pairing snapshot to the wire event.
        fn pairing_event(s: crate::pairing::PairState) -> UiEvent {
            use crate::pairing::PairState::*;
            let (phase, sas, peer) = match s {
                Idle => ("idle", String::new(), String::new()),
                Discoverable => ("discoverable", String::new(), String::new()),
                Connecting => ("connecting", String::new(), String::new()),
                Prompt { sas, peer } => ("prompt", sas, peer),
                Done { ok: true, peer } => ("paired", String::new(), peer),
                Done { ok: false, peer } => ("aborted", String::new(), peer),
                Error(e) => ("error", String::new(), e),
            };
            UiEvent::Pairing { phase: phase.into(), sas, peer }
        }

        // Map core monitors to the arranger's wire view (`dev` 0 = us, 1 = peer).
        fn monitor_views(
            monitors: &[deskoryn_core::layout::Monitor],
            me: deskoryn_core::DeviceId,
        ) -> Vec<crate::ipc::MonitorView> {
            monitors
                .iter()
                .map(|m| crate::ipc::MonitorView {
                    dev: if m.device() == me { 0 } else { 1 },
                    index: m.id.index,
                    label: m.label.clone(),
                    x: m.bounds.x,
                    y: m.bounds.y,
                    w: m.bounds.w,
                    h: m.bounds.h,
                })
                .collect()
        }

        // Build a fresh Status snapshot: trusted devices with live connection
        // state from the registry, plus this device's own detected monitors.
        async fn status(
            trust: &tokio::sync::Mutex<deskoryn_core::trust::TrustStore>,
            registry: &crate::session::ConnRegistry,
            device_name: String,
            device_id: deskoryn_core::DeviceId,
            port: u16,
        ) -> Vec<UiEvent> {
            let connected: std::collections::HashSet<deskoryn_core::DeviceId> =
                registry.lock().await.keys().copied().collect();
            let t = trust.lock().await;
            let peers = t
                .devices
                .iter()
                .map(|d| PeerStatus {
                    name: d.name.clone(),
                    device: d.id.to_string(),
                    connected: connected.contains(&d.id),
                    address: d.last_address.clone(),
                    latency_ms: None,
                })
                .collect();
            let monitors = crate::monitors::detect_monitors(device_id)
                .map(|m| monitor_views(&m, device_id))
                .unwrap_or_default();
            vec![UiEvent::Status {
                device_name,
                device_id: device_id.to_string(),
                peers,
                active: !connected.is_empty(),
                port,
                addrs: local_addrs(port),
                monitors,
            }]
        }

        let handler: ipc::Handler = std::sync::Arc::new(move |req| {
            let trust = trust_for_ipc.clone();
            let device_name = device_name.clone();
            let trust_file = trust_file.clone();
            let pairing = pairing_ipc.clone();
            let endpoint = endpoint_ipc.clone();
            let config = config_ipc.clone();
            let paths = paths_ipc.clone();
            let discovered = discovered_ipc.clone();
            let registry = registry_ipc.clone();
            let layouts = layouts_ipc.clone();
            Box::pin(async move {
                match req {
                    UiRequest::Status => {
                        status(&trust, &registry, device_name, device_id, listen_port).await
                    }
                    UiRequest::Arrangement { peer } => {
                        // Look up the connected peer for its real monitors; build
                        // own + theirs + the saved-or-seeded combined layout.
                        let Ok(target) = peer.parse::<deskoryn_core::DeviceId>() else {
                            return vec![];
                        };
                        let reg = registry.lock().await;
                        let Some(info) = reg.get(&target) else {
                            return vec![]; // not connected → arranger stays disabled
                        };
                        let own = crate::monitors::detect_monitors(device_id).unwrap_or_default();
                        let layout = {
                            let store = layouts.lock().await;
                            store.get(&target).cloned()
                        }
                        .unwrap_or_else(|| crate::session::seed_layout(&own, &info.monitors));
                        vec![UiEvent::Arrangement {
                            peer,
                            own: monitor_views(&own, device_id),
                            theirs: monitor_views(&info.monitors, device_id),
                            layout,
                        }]
                    }
                    UiRequest::SetLayout { peer, layout } => {
                        let Ok(target) = peer.parse::<deskoryn_core::DeviceId>() else {
                            return vec![];
                        };
                        // Persist: update the in-memory store, then rewrite the
                        // config file's peer_layouts from it.
                        {
                            let mut store = layouts.lock().await;
                            store.insert(target, layout.clone());
                            let mut cfg = (*config).clone();
                            cfg.set_layout_for(target, layout.clone());
                            if let Err(e) = cfg.save(&paths.config_file()) {
                                tracing::warn!(error = %e, "failed to persist peer layout");
                            }
                        }
                        // Apply live if the peer is connected (best-effort).
                        if let Some(info) = registry.lock().await.get(&target) {
                            let _ = info.layout_tx.send(layout).await;
                        }
                        status(&trust, &registry, device_name, device_id, listen_port).await
                    }
                    UiRequest::DiscoveredPeers => {
                        use crate::ipc::DiscoveredPeer;
                        let now = std::time::Instant::now();
                        let d = discovered.lock().await;
                        let t = trust.lock().await;
                        let peers = d
                            .iter()
                            // mDNS re-announces on the pair-flag toggle and on TTL
                            // refresh (~minutes), not every few seconds — so keep
                            // entries for a TTL-sized window. Closing the window
                            // re-announces pair=0, which removes it promptly.
                            .filter(|(_, e)| {
                                e.pairing
                                    && now.duration_since(e.last_seen) < std::time::Duration::from_secs(150)
                            })
                            .map(|(id, e)| DiscoveredPeer {
                                name: e.name.clone(),
                                addr: e.addrs.first().map(|a| a.to_string()).unwrap_or_default(),
                                device: id.short(),
                                trusted: t.get(*id).is_some(),
                            })
                            .collect();
                        vec![UiEvent::Discovered { peers }]
                    }
                    UiRequest::PairStatus => vec![pairing_event(pairing.snapshot().await)],
                    UiRequest::PairConfirm { accept } => {
                        pairing.respond(accept).await;
                        vec![pairing_event(pairing.snapshot().await)]
                    }
                    UiRequest::PairCancel => {
                        pairing.clear().await;
                        vec![pairing_event(pairing.snapshot().await)]
                    }
                    UiRequest::Pair { addr } => {
                        if addr.trim().is_empty() {
                            // Open the discoverable window; the accept loop will
                            // route the next untrusted peer into pairing. Check
                            // now whether inbound is likely blocked and warn.
                            pairing.set_discoverable(true).await;
                            let mut events = vec![pairing_event(pairing.snapshot().await)];
                            if let Some(text) = inbound_firewall_hint() {
                                events.push(UiEvent::Notice {
                                    level: crate::ipc::NoticeLevel::Warning,
                                    text,
                                });
                            }
                            return events;
                        } else {
                            // Dial the chosen peer and pair, on the live endpoint.
                            let addr = addr.trim().to_string();
                            match addr.parse::<std::net::SocketAddr>() {
                                Ok(sock) => {
                                    // If this is a discovered peer, try ALL of its
                                    // advertised addresses (a stray APIPA/Docker
                                    // address shouldn't be the only attempt).
                                    let candidates: Vec<std::net::SocketAddr> = {
                                        let d = discovered.lock().await;
                                        d.values()
                                            .find(|e| e.addrs.contains(&sock))
                                            .map(|e| e.addrs.clone())
                                            .unwrap_or_else(|| vec![sock])
                                    };
                                    let pairing2 = pairing.clone();
                                    let config2 = config.clone();
                                    let trust2 = trust.clone();
                                    let paths2 = paths.clone();
                                    let endpoint2 = endpoint.clone();
                                    let registry2 = registry.clone();
                                    let layouts2 = layouts.clone();
                                    tokio::spawn(async move {
                                        let mut last_err = String::from("no address");
                                        for cand in &candidates {
                                            // Bound each attempt so an unreachable
                                            // address fails fast and we try the next.
                                            match tokio::time::timeout(
                                                std::time::Duration::from_secs(5),
                                                endpoint2.connect_unverified(*cand),
                                            )
                                            .await
                                            {
                                                Ok(Ok(session)) => {
                                                    let addr = cand.to_string();
                                                    let _ = crate::pairing::run_handshake(
                                                        pairing2.clone(), session, true,
                                                        Some(addr.clone()), config2.clone(),
                                                        local_fp_ipc, trust2.clone(), paths2.clone(),
                                                    )
                                                    .await;
                                                    // On success, dial a real session right away so the
                                                    // two daemons connect now (not only after a restart).
                                                    if matches!(
                                                        pairing2.snapshot().await,
                                                        crate::pairing::PairState::Done { ok: true, .. }
                                                    ) {
                                                        spawn_peer_dial(
                                                            endpoint2.clone(), addr, config2.clone(),
                                                            registry2.clone(), layouts2.clone(),
                                                            paths2.config_file(),
                                                        );
                                                    }
                                                    return;
                                                }
                                                Ok(Err(e)) => last_err = e.to_string(),
                                                Err(_) => last_err = "timed out".into(),
                                            }
                                        }
                                        // Reactive firewall hint after all tries failed.
                                        pairing2.fail(format!(
                                            "couldn't reach {addr} — the other device may be offline or blocking inbound \
                                             (firewall). Make sure its daemon is running and inbound UDP is allowed. \
                                             (tried {} address(es); last: {last_err})",
                                            candidates.len()
                                        )).await;
                                    });
                                }
                                Err(e) => pairing.fail(format!("bad address: {e}")).await,
                            }
                        }
                        vec![pairing_event(pairing.snapshot().await)]
                    }
                    UiRequest::Forget { device } => {
                        {
                            let mut t = trust.lock().await;
                            // The UI sends the device's name; map it to its id.
                            if let Some(id) = t.devices.iter().find(|d| d.name == device).map(|d| d.id) {
                                if t.forget(id) {
                                    if let Err(e) = t.save(&trust_file) {
                                        tracing::warn!(error = %e, "failed to save trust store after forget");
                                    }
                                    tracing::info!(%device, "forgot trusted device");
                                }
                            }
                        }
                        // Return a fresh snapshot so the UI updates immediately.
                        status(&trust, &registry, device_name, device_id, listen_port).await
                    }
                    _ => vec![],
                }
            }) as HandlerFuture
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
        match start_discovery(&config, &endpoint, &trust, identity_fp, &pairing, &discovered, &registry, &layouts, paths.config_file()).await {
            Ok(()) => tracing::info!(name = %config.device.name, "advertising on mDNS"),
            Err(e) => tracing::warn!(error = %e, "mDNS discovery unavailable"),
        }
    }

    // Accept loop: a trusted peer gets a session; an untrusted peer is paired
    // (only while a discoverable window is open), else dropped.
    {
        let endpoint = endpoint.clone();
        let config = config.clone();
        let pairing = pairing.clone();
        let trust_accept = trust.clone();
        let paths_accept = paths.clone();
        let registry_accept = registry.clone();
        let layouts_accept = layouts.clone();
        tokio::spawn(async move {
            use deskoryn_net::quic::Accepted;
            loop {
                match endpoint.accept_any().await {
                    Ok(Accepted::Trusted(session)) => {
                        let config = config.clone();
                        let registry = registry_accept.clone();
                        let layouts = layouts_accept.clone();
                        let config_path = paths_accept.config_file();
                        tokio::spawn(async move {
                            if let Err(e) = crate::session::run(config, session, registry, layouts, config_path).await {
                                tracing::warn!(error = %e, "inbound session ended");
                            }
                        });
                    }
                    Ok(Accepted::Unknown(session)) => {
                        if pairing.is_discoverable() {
                            let pairing = pairing.clone();
                            let config = config.clone();
                            let trust = trust_accept.clone();
                            let paths = paths_accept.clone();
                            tokio::spawn(async move {
                                if let Err(e) = crate::pairing::run_handshake(
                                    pairing, session, false, None, config, identity_fp, trust, paths,
                                )
                                .await
                                {
                                    tracing::warn!(error = %e, "pairing handshake failed");
                                }
                            });
                        } else {
                            tracing::debug!("dropped untrusted peer (not in pairing mode)");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "accept failed");
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
        spawn_peer_dial(
            endpoint.clone(),
            addr_str,
            config.clone(),
            registry.clone(),
            layouts.clone(),
            paths.config_file(),
        );
    }

    // The spawned loops do the work; park until shutdown.
    std::future::pending::<()>().await;
    Ok(())
}

/// Spawn a reconnecting dial task to one peer address: connect, run the session,
/// and re-dial with backoff whenever it drops. Used for known peers at startup
/// and, crucially, right after a successful pairing so the two daemons connect
/// immediately instead of only after a restart.
#[cfg(any(feature = "linux", feature = "windows"))]
fn spawn_peer_dial(
    endpoint: Arc<deskoryn_net::quic::QuicEndpoint>,
    addr_str: String,
    config: Arc<AppConfig>,
    registry: crate::session::ConnRegistry,
    layouts: crate::session::LayoutStore,
    config_path: std::path::PathBuf,
) {
    tokio::spawn(async move {
        let mut backoff = Backoff::default();
        loop {
            match addr_str.parse() {
                Ok(addr) => match endpoint.connect_any(addr).await {
                    Ok(session) => {
                        backoff = Backoff::default();
                        if let Err(e) = crate::session::run(
                            config.clone(),
                            session,
                            registry.clone(),
                            layouts.clone(),
                            config_path.clone(),
                        )
                        .await
                        {
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

/// Start mDNS: advertise this device and dial discovered, **already-trusted**
/// peers. To avoid both peers dialing each other, only the device with the
/// lexicographically smaller id initiates; the other waits to be accepted.
#[cfg(any(feature = "linux", feature = "windows"))]
async fn start_discovery(
    config: &Arc<AppConfig>,
    endpoint: &Arc<deskoryn_net::quic::QuicEndpoint>,
    trust: &Arc<tokio::sync::Mutex<deskoryn_core::trust::TrustStore>>,
    fingerprint: deskoryn_core::trust::CertFingerprint,
    pairing: &Arc<crate::pairing::Pairing>,
    discovered: &Arc<Discovered>,
    registry: &crate::session::ConnRegistry,
    layouts: &crate::session::LayoutStore,
    config_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    use deskoryn_net::discovery::{mdns::MdnsDiscovery, Discovery};

    let discovery = Arc::new(MdnsDiscovery::new()?);
    discovery
        .advertise(config.device.id, &config.device.name, endpoint.local_port(), fingerprint)
        .await?;
    // Let the pairing coordinator toggle the advertised "accepting pairing" flag.
    pairing.set_discovery(discovery.clone());

    let endpoint = endpoint.clone();
    let config = config.clone();
    let trust = trust.clone();
    let discovered = discovered.clone();
    let registry = registry.clone();
    let layouts = layouts.clone();
    // Peers we already have a session with (or are dialing), so repeated mDNS
    // re-resolutions don't spawn a storm of duplicate connections.
    let active: Arc<tokio::sync::Mutex<std::collections::HashSet<deskoryn_core::DeviceId>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    tokio::spawn(async move {
        while let Some(hint) = discovery.next_peer().await {
            // Record every sighting (trusted or not) for the UI's nearby list.
            {
                let mut d = discovered.lock().await;
                d.insert(
                    hint.device,
                    DiscEntry {
                        name: hint.name.clone(),
                        addrs: hint.addrs.clone(),
                        pairing: hint.pairing,
                        last_seen: std::time::Instant::now(),
                    },
                );
            }
            let trusted = trust.lock().await.get(hint.device).is_some();
            if !trusted {
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
                    let registry = registry.clone();
                    let layouts = layouts.clone();
                    let config_path = config_path.clone();
                    let device = hint.device;
                    tokio::spawn(async move {
                        if let Err(e) = crate::session::run(config, session, registry, layouts, config_path).await {
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
async fn run_dry(config: Arc<AppConfig>, config_path: std::path::PathBuf) -> anyhow::Result<()> {
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

    // Fresh, empty registry + saved-layout store for the dry run.
    let registry: crate::session::ConnRegistry =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let layouts: crate::session::LayoutStore =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    crate::session::run(config, Box::new(mine), registry, layouts, config_path).await?;
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
