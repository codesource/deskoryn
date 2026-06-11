//! Top-level supervision: bring up endpoint + discovery, then spawn and restart
//! a [`session`](crate::session) task per connected peer. Implements the
//! "reconnect after sleep / reboot / network blip" requirement by treating a
//! dropped session as normal and re-dialing with backoff.

use deskoryn_core::config::{AppConfig, Paths};
use std::sync::Arc;
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

    // --- Real path (requires the `linux`/`windows` build features) -----------
    //
    // TODO(impl):
    //  1. load-or-generate the device identity (quic::DeviceIdentity), persist
    //     under paths.{key,cert}_file();
    //  2. load the trust store (paths.trust_file());
    //  3. bind a QuicEndpoint and, if enabled, start mDNS advertise/browse;
    //  4. loop: accept inbound sessions and dial known/discovered peers; for each
    //     established Session, spawn session::run; on error, reconnect with
    //     capped exponential backoff (see `backoff` below).
    let _ = (&paths,);
    let mut backoff = Backoff::default();
    loop {
        tracing::warn!("real transport not built in (enable the `linux`/`windows` feature); idling");
        tokio::time::sleep(backoff.next()).await;
    }
}

/// Run the whole daemon in one process over a loopback session, with a synthetic
/// peer. Exercises the orchestration without touching the OS or network.
async fn run_dry(config: Arc<AppConfig>) -> anyhow::Result<()> {
    use deskoryn_net::transport::{loopback, Session};
    use deskoryn_proto::{Capabilities, Control, PROTOCOL_VERSION};

    let me = config.device.id;
    let peer = deskoryn_core::DeviceId::from_bytes([0xEE; 16]);
    let (mine, theirs) = loopback::loopback(me, peer);

    // Pretend the peer is another daemon: echo a Hello back.
    let peer_task = tokio::spawn(async move {
        let mut buf = bytes::BytesMut::new();
        let (mut sink, mut src) = theirs.channel(deskoryn_proto::Channel::Control).await.unwrap();
        // Wait for our Hello, then reply.
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
            deskoryn_proto::encode(&reply, &mut buf).unwrap();
            let _ = sink.send_bytes(&buf).await;
        }
    });

    crate::session::run(config, Box::new(mine)).await?;
    let _ = peer_task.await;
    Ok(())
}

/// Capped exponential backoff for reconnection.
struct Backoff {
    current: Duration,
    max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            current: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }
}
