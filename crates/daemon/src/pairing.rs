//! Daemon-integrated pairing.
//!
//! Pairing runs on the daemon's **live** QUIC endpoint, so you can pair while
//! the daemon is connected (no separate `deskorynd pair` process, no port
//! conflict). A user-opened *discoverable* window lets the accept loop route an
//! untrusted inbound connection here; to pair the other direction the daemon
//! dials a peer. Either way the 6-digit SAS is surfaced over the control socket
//! and the user's confirm comes back the same way — the key exchange / compare
//! is unchanged (see `docs/SECURITY.md`), only the transport for the prompt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use deskoryn_core::config::{AppConfig, Paths};
use deskoryn_core::trust::{CertFingerprint, TrustStore};
use deskoryn_net::discovery::Discovery;
use deskoryn_net::quic::QuicSession;
use tokio::sync::{oneshot, Mutex};

/// Snapshot of the pairing flow for the UI (no internal handles).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PairState {
    #[default]
    Idle,
    /// Discoverable window open, waiting for a peer to connect.
    Discoverable,
    /// Dialing a chosen peer.
    Connecting,
    /// Compare-and-confirm: the same code must show on both machines.
    Prompt { sas: String, peer: String },
    Done { ok: bool, peer: String },
    Error(String),
}

struct Inner {
    state: PairState,
    /// Resolves the in-flight SAS prompt (Some only during `Prompt`).
    confirm: Option<oneshot::Sender<bool>>,
    /// A handshake is in progress; refuse a second one.
    busy: bool,
}

/// Shared pairing coordinator: held by the supervisor (accept loop + dial) and
/// the IPC handler.
pub struct Pairing {
    discoverable: AtomicBool,
    inner: Mutex<Inner>,
    /// mDNS handle, so opening/closing the window also toggles the advertised
    /// "accepting pairing" flag (set once discovery starts).
    discovery: std::sync::Mutex<Option<Arc<dyn Discovery>>>,
}

impl Default for Pairing {
    fn default() -> Self {
        Self {
            discoverable: AtomicBool::new(false),
            inner: Mutex::new(Inner { state: PairState::Idle, confirm: None, busy: false }),
            discovery: std::sync::Mutex::new(None),
        }
    }
}

impl Pairing {
    pub fn is_discoverable(&self) -> bool {
        self.discoverable.load(Ordering::SeqCst)
    }

    /// Attach the mDNS handle so the window toggles the advertised flag.
    pub fn set_discovery(&self, d: Arc<dyn Discovery>) {
        *self.discovery.lock().unwrap() = Some(d);
    }

    /// Toggle the advertised "accepting pairing" flag (best-effort).
    async fn advertise(&self, on: bool) {
        let d = self.discovery.lock().unwrap().clone();
        if let Some(d) = d {
            let _ = d.set_pairing(on).await;
        }
    }

    /// Open/close the discoverable window (set by the UI's "Start pairing").
    pub async fn set_discoverable(&self, on: bool) {
        self.discoverable.store(on, Ordering::SeqCst);
        {
            let mut i = self.inner.lock().await;
            if on {
                if matches!(i.state, PairState::Idle | PairState::Done { .. } | PairState::Error(_)) {
                    i.state = PairState::Discoverable;
                }
            } else if matches!(i.state, PairState::Discoverable) {
                i.state = PairState::Idle;
            }
        }
        self.advertise(on).await;
    }

    pub async fn snapshot(&self) -> PairState {
        self.inner.lock().await.state.clone()
    }

    /// Record a failure (e.g. dial error) and end any in-flight flow.
    pub async fn fail(&self, msg: String) {
        self.discoverable.store(false, Ordering::SeqCst);
        {
            let mut i = self.inner.lock().await;
            i.confirm = None;
            i.busy = false;
            i.state = PairState::Error(msg);
        }
        self.advertise(false).await;
    }

    /// Answer the in-flight SAS prompt.
    pub async fn respond(&self, accept: bool) {
        if let Some(tx) = self.inner.lock().await.confirm.take() {
            let _ = tx.send(accept);
        }
    }

    /// Cancel/dismiss: drop any in-flight prompt and reset to idle (also closes
    /// the discoverable window).
    pub async fn clear(&self) {
        self.discoverable.store(false, Ordering::SeqCst);
        {
            let mut i = self.inner.lock().await;
            if let Some(tx) = i.confirm.take() {
                let _ = tx.send(false);
            }
            i.state = PairState::Idle;
            i.busy = false;
        }
        self.advertise(false).await;
    }
}

/// Run one pairing handshake to completion on `session` (already connected).
/// `initiator` = we dialed; otherwise we accepted. Surfaces the SAS via the
/// coordinator, waits for the IPC confirm, and pins trust on success.
pub async fn run_handshake(
    pairing: Arc<Pairing>,
    session: QuicSession,
    initiator: bool,
    last_address: Option<String>,
    config: Arc<AppConfig>,
    local_fp: CertFingerprint,
    trust: Arc<Mutex<TrustStore>>,
    paths: Paths,
) -> anyhow::Result<()> {
    // Single pairing at a time.
    {
        let mut i = pairing.inner.lock().await;
        if i.busy {
            anyhow::bail!("a pairing is already in progress");
        }
        i.busy = true;
        i.state = if initiator { PairState::Connecting } else { PairState::Discoverable };
    }
    // Accepting/handshaking one peer is enough — stop advertising.
    pairing.discoverable.store(false, Ordering::SeqCst);
    pairing.advertise(false).await;

    let outcome = handshake_inner(&pairing, &session, last_address, &config, local_fp, &trust, &paths).await;

    let mut i = pairing.inner.lock().await;
    i.busy = false;
    i.confirm = None;
    match &outcome {
        Ok((ok, peer)) => i.state = PairState::Done { ok: *ok, peer: peer.clone() },
        Err(e) => i.state = PairState::Error(e.to_string()),
    }
    outcome.map(|_| ())
}

async fn handshake_inner(
    pairing: &Arc<Pairing>,
    session: &QuicSession,
    last_address: Option<String>,
    config: &AppConfig,
    local_fp: CertFingerprint,
    trust: &Arc<Mutex<TrustStore>>,
    paths: &Paths,
) -> anyhow::Result<(bool, String)> {
    use deskoryn_net::pairing::PairingSession;

    let (peer_id, peer_name) = exchange(session, config.device.id, &config.device.name).await?;
    let cb = session.channel_binding()?;
    let ps = PairingSession::new(
        config.device.id,
        local_fp,
        peer_id,
        session.fingerprint(),
        peer_name.clone(),
        &cb,
    );

    // Surface the SAS and wait (bounded) for the user's confirm over IPC.
    let (tx, rx) = oneshot::channel::<bool>();
    {
        let mut i = pairing.inner.lock().await;
        i.state = PairState::Prompt { sas: ps.sas.display(), peer: peer_name.clone() };
        i.confirm = Some(tx);
    }
    let accept = matches!(
        tokio::time::timeout(Duration::from_secs(180), rx).await,
        Ok(Ok(true))
    );
    if !accept {
        return Ok((false, peer_name));
    }

    {
        let mut t = trust.lock().await;
        t.upsert(ps.confirm(now_unix(), last_address));
        t.save(&paths.trust_file())?;
    }
    tracing::info!(peer = %peer_name, "paired (daemon)");
    Ok((true, peer_name))
}

/// Exchange `Control::Hello`; return the peer's id + name.
async fn exchange(
    session: &QuicSession,
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
