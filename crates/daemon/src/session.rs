//! Per-peer session driver.
//!
//! Owns one [`Session`] and runs its logical-channel pumps concurrently:
//!
//! * **Control**: handshake (`Hello`), heartbeat (`Ping`/`Pong` → drives
//!   reconnect detection), and `LayoutUpdate` (see [`crate::control`]).
//! * **Input**: capture local input, forward across the boundary, inject what the
//!   peer forwards (see [`crate::input`]).
//! * **Clipboard / FileXfer / Audio**: bridged by their pump modules; triggered
//!   by config / UI in later milestones.

use deskoryn_core::config::AppConfig;
use deskoryn_core::layout::{Monitor, VirtualDesktop};
use deskoryn_core::DeviceId;
use deskoryn_net::transport::Session;
use deskoryn_proto::{Channel, Control, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Live info about one connected peer, shared with the IPC/arranger so the UI can
/// show the peer's real monitors and push a new arrangement to the live session.
pub struct ConnInfo {
    /// The peer's monitors (in their own OS coordinates), from their `Hello`.
    pub monitors: Vec<Monitor>,
    /// Push a re-arranged combined layout to this session's input pump.
    pub layout_tx: mpsc::Sender<VirtualDesktop>,
}

/// Currently-connected peers, keyed by device id. Empty ⇒ no peer connected
/// (the arranger is disabled and shows only this device's monitors).
pub type ConnRegistry = Arc<Mutex<HashMap<DeviceId, ConnInfo>>>;

/// Saved per-peer combined arrangements, loaded from config at startup and
/// persisted back on `SetLayout`. Read at connect time to restore the layout.
pub type LayoutStore = Arc<Mutex<HashMap<DeviceId, VirtualDesktop>>>;

/// Auto-place the peer's monitors immediately to the right of ours, producing a
/// combined desktop to edit when there is no saved arrangement for this peer yet.
pub fn seed_layout(own: &[Monitor], peer: &[Monitor]) -> VirtualDesktop {
    let mut monitors = own.to_vec();
    let right = own.iter().map(|m| m.bounds.right()).max().unwrap_or(0);
    let peer_left = peer.iter().map(|m| m.bounds.left()).min().unwrap_or(0);
    let shift = right - peer_left;
    for m in peer {
        let mut m = m.clone();
        m.bounds.x += shift;
        monitors.push(m);
    }
    VirtualDesktop::new(monitors)
}

pub async fn run(
    config: Arc<AppConfig>,
    session: Box<dyn Session>,
    registry: ConnRegistry,
    layouts: LayoutStore,
    config_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    // Shared so the clipboard pump can open the FileXfer channel for file-paste
    // while the input pump keeps using the same session.
    let session: Arc<dyn Session> = Arc::from(session);
    let peer = session.peer();
    tracing::info!(%peer, "session established");

    // This device's monitors, freshly detected (with resolutions), to advertise
    // in our Hello. Fall back to whatever the config recorded for us if the OS
    // can't be queried (e.g. a Wayland session without a backend).
    let own_monitors = crate::monitors::detect_monitors(config.device.id).unwrap_or_else(|e| {
        tracing::debug!(error = %e, "monitor auto-detect unavailable; using configured monitors");
        config
            .layout
            .monitors
            .iter()
            .filter(|m| m.device() == config.device.id)
            .cloned()
            .collect()
    });

    // --- Handshake on the Control channel -----------------------------------
    let (mut ctl_tx, mut ctl_rx) = session.channel(Channel::Control).await?;
    let mut buf = bytes::BytesMut::new();
    let hello = Control::Hello {
        version: PROTOCOL_VERSION,
        device: config.device.id,
        name: config.device.name.clone(),
        monitors: VirtualDesktop::new(own_monitors.clone()),
        capabilities: capabilities_from(&config),
    };
    deskoryn_proto::encode(&hello, &mut buf)?;
    ctl_tx.send_bytes(&buf).await?;

    // Read the peer's Hello: capture their monitors and audio capability.
    let mut peer_monitors: Vec<Monitor> = Vec::new();
    let mut peer_forwards_audio = false;
    if let Some(frame) = ctl_rx.recv_bytes().await? {
        let mut b = bytes::BytesMut::from(&frame[..]);
        if let Some(Control::Hello { name, monitors, version, capabilities, .. }) =
            deskoryn_proto::decode_one::<Control>(&mut b)?
        {
            tracing::info!(peer_name = %name, ?version, monitors = monitors.monitors.len(), "peer hello");
            peer_monitors = monitors.monitors;
            peer_forwards_audio = capabilities.audio_forward;
        }
    }

    // The combined desktop: the user's saved arrangement for this peer if any,
    // else auto-place the peer to the right of us.
    let layout = {
        let store = layouts.lock().await;
        store.get(&peer).cloned()
    }
    .unwrap_or_else(|| seed_layout(&own_monitors, &peer_monitors));

    // Layout wiring. `local_*` carries arranger applies from the IPC handler into
    // the control pump (which broadcasts them to the peer); `apply_*` carries the
    // effective layout — local or peer-synced — into the input pump's controller.
    let (local_tx, local_rx) = mpsc::channel::<VirtualDesktop>(4);
    let (apply_tx, apply_rx) = mpsc::channel::<VirtualDesktop>(4);
    {
        let mut reg = registry.lock().await;
        reg.insert(peer, ConnInfo { monitors: peer_monitors.clone(), layout_tx: local_tx });
    }

    // Build the input controller over the combined virtual desktop, starting the
    // cursor on one of our own monitors.
    let start = start_position(&layout, config.device.id);
    let controller = crate::input::Controller::new(layout, config.device.id, start)
        .with_input_config(&config.input)
        // Our detected monitors (local OS coords) let an incoming Enter warp the
        // cursor to exactly where the pointer crossed in.
        .with_local_monitors(own_monitors.clone());
    let capture = deskoryn_input::platform::open_capture()?;
    let injector = deskoryn_input::platform::open_injector()?;
    tracing::info!(backend = ?deskoryn_input::platform::detect(), "input backend");

    // --- Concurrent channel pumps -------------------------------------------
    //
    // Run the control pump (heartbeat + control messages) and the input pump
    // (capture -> forward / inject) for the lifetime of the session; whichever
    // ends first tears the session down so the supervisor can reconnect.
    tracing::info!(%peer, "session ready; starting pumps");
    let layout_sync = crate::control::LayoutSync {
        peer,
        local_rx,
        apply_tx,
        layouts: layouts.clone(),
        config: config.clone(),
        config_path: config_path.clone(),
    };
    let control = crate::control::run_control(
        ctl_tx,
        ctl_rx,
        crate::control::HeartbeatConfig::default(),
        Some(layout_sync),
    );
    let input = crate::input::run_input(session.as_ref(), controller, capture, injector, apply_rx);

    // Clipboard sync (text + images on the Clipboard channel; files stream over
    // dedicated streams). Skipped entirely when all clipboard sync is disabled;
    // with the portable/no-op backend the pump simply parks (idle stream).
    let clip = &config.clipboard;
    type PumpFuture = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;
    type ClipAccess = std::sync::Arc<dyn deskoryn_clipboard::ClipboardAccess>;
    let (clip_access, clipboard): (Option<ClipAccess>, PumpFuture) =
        if clip.sync_text || clip.sync_images || clip.sync_files {
            let (clip_sink, clip_source) = session.channel(Channel::Clipboard).await?;
            let (access, clip_changes) = deskoryn_clipboard::platform::open_access(
                std::time::Duration::from_millis(clip.poll_ms),
            );
            let fut = Box::pin(crate::clipboard::run_clipboard(
                access.clone(),
                clip_changes,
                clip_sink,
                clip_source,
                clip.inline_max_bytes,
                session.clone(),
                clip.sync_files,
            ));
            (Some(access), fut)
        } else {
            (None, Box::pin(std::future::pending()))
        };

    // Dispatcher: accepts dedicated streams (file transfers + clipboard
    // file-paste) for the session lifetime, each handled concurrently.
    let download = clip.sync_files.then(|| {
        (
            config
                .file_transfer
                .download_dir
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("deskoryn-received")),
            config.file_transfer.conflict_policy,
        )
    });
    let dispatcher: PumpFuture = Box::pin(crate::transfer::run_dispatcher(
        session.clone(),
        clip_access,
        download,
        config.file_transfer.max_concurrent_transfers,
    ));

    // Audio forwarding (single direction, role by config): a machine with
    // `audio.forward_enabled` captures and streams its output; otherwise it
    // plays what a forwarding peer sends. Parks (without ending the session)
    // when this machine has no audio role or no backend. Uses QUIC datagrams,
    // so it needs the `audio-opus` codec to fit frames into a datagram.
    let audio: PumpFuture = Box::pin(crate::audio::run_audio_pump(
        session.clone(),
        config.audio.profile,
        config.audio.forward_enabled,
        peer_forwards_audio,
    ));

    let result = tokio::select! {
        r = control => { r.map(|e| format!("{e:?}")) }
        r = input => { r.map(|_| "input pump ended".to_string()) }
        r = clipboard => { r.map(|_| "clipboard pump ended".to_string()) }
        r = dispatcher => { r.map(|_| "stream dispatcher ended".to_string()) }
        r = audio => { r.map(|_| "audio pump ended".to_string()) }
    };

    // Drop this peer from the live registry however the session ends, so the
    // arranger reflects the disconnect (and falls back to own-monitors-only).
    registry.lock().await.remove(&peer);

    let end = result?;
    tracing::info!(%peer, %end, "session ended");
    Ok(())
}

/// Where to place the tracked cursor when a session starts.
///
/// We start on the monitor this device owns that sits **closest to a peer
/// monitor** — the "gateway" the cursor crosses through — rather than the first
/// one listed. Otherwise a machine whose first monitor is far from the boundary
/// (e.g. a 3-wide row where the peer is off the right edge) would require a huge
/// drag before the first hand-off, while the other side, starting next to the
/// boundary, crosses instantly. Centering on the gateway makes the first
/// crossing short and symmetric in both directions.
fn start_position(layout: &deskoryn_core::VirtualDesktop, me: deskoryn_core::DeviceId) -> deskoryn_core::geometry::Point {
    use deskoryn_core::geometry::{Point, Rect};
    let center = |b: &Rect| Point::new(b.x + b.w / 2, b.y + b.h / 2);
    let mine = || layout.monitors.iter().filter(|m| m.device() == me);
    let peers: Vec<Rect> = layout.monitors.iter().filter(|m| m.device() != me).map(|m| m.bounds).collect();

    // Among my monitors, pick the one with the smallest gap to any peer monitor.
    let gateway = mine().min_by_key(|m| {
        peers.iter().map(|p| rect_gap(m.bounds, *p)).min().unwrap_or(i32::MAX)
    });
    gateway
        .or_else(|| mine().next())
        .map(|m| center(&m.bounds))
        .unwrap_or(Point::new(0, 0))
}

/// Manhattan gap between two rectangles (0 when they touch or overlap).
fn rect_gap(a: deskoryn_core::geometry::Rect, b: deskoryn_core::geometry::Rect) -> i32 {
    let dx = (b.left() - a.right()).max(a.left() - b.right()).max(0);
    let dy = (b.top() - a.bottom()).max(a.top() - b.bottom()).max(0);
    dx + dy
}

fn capabilities_from(config: &AppConfig) -> deskoryn_proto::Capabilities {
    deskoryn_proto::Capabilities {
        clipboard_text: config.clipboard.sync_text,
        clipboard_images: config.clipboard.sync_images,
        clipboard_files: config.clipboard.sync_files,
        file_transfer: true,
        audio_forward: config.audio.forward_enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::{rect_gap, start_position};
    use deskoryn_core::geometry::{Point, Rect, Size};
    use deskoryn_core::layout::{Monitor, VirtualDesktop};
    use deskoryn_core::{DeviceId, MonitorId};

    fn dev(b: u8) -> DeviceId {
        DeviceId::from_bytes([b; 16])
    }

    fn mon(device: DeviceId, idx: u16, x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor {
            id: MonitorId::new(device, idx),
            label: format!("m{idx}"),
            bounds: Rect::new(x, y, w, h),
            native: Size::new(w, h),
            scale_pct: 100,
        }
    }

    /// Three Linux monitors on the left, two Windows monitors on the right —
    /// the boundary sits between Lin-R (x≈3840..5760) and Win-L (x=5760).
    fn sample() -> (DeviceId, DeviceId, VirtualDesktop) {
        let lin = dev(1);
        let win = dev(2);
        let vd = VirtualDesktop::new(vec![
            mon(lin, 0, 0, 0, 1920, 1080),
            mon(lin, 1, 1920, 0, 1920, 1080),
            mon(lin, 2, 3840, 0, 1920, 1080),
            mon(win, 0, 5760, 0, 2560, 1440),
            mon(win, 1, 8320, 0, 2560, 1440),
        ]);
        (lin, win, vd)
    }

    #[test]
    fn starts_on_gateway_monitor_not_first() {
        let (lin, win, vd) = sample();
        // Linux's gateway is Lin-R (touches Win-L), centered at 3840 + 960 = 4800.
        assert_eq!(start_position(&vd, lin), Point::new(4800, 540));
        // Windows's gateway is Win-L (touches Lin-R), centered at 5760 + 1280 = 7040.
        assert_eq!(start_position(&vd, win), Point::new(7040, 720));
    }

    #[test]
    fn falls_back_to_origin_with_no_monitors() {
        let vd = VirtualDesktop::new(vec![]);
        assert_eq!(start_position(&vd, dev(1)), Point::new(0, 0));
    }

    #[test]
    fn falls_back_to_first_owned_when_no_peers() {
        let lin = dev(1);
        let vd = VirtualDesktop::new(vec![
            mon(lin, 0, 0, 0, 1920, 1080),
            mon(lin, 1, 1920, 0, 1920, 1080),
        ]);
        // No peer monitors → all gaps are i32::MAX, min_by_key keeps the first.
        assert_eq!(start_position(&vd, lin), Point::new(960, 540));
    }

    #[test]
    fn rect_gap_is_zero_when_touching_or_overlapping() {
        let a = Rect::new(0, 0, 100, 100);
        assert_eq!(rect_gap(a, Rect::new(100, 0, 50, 100)), 0); // edge-to-edge
        assert_eq!(rect_gap(a, Rect::new(50, 50, 100, 100)), 0); // overlapping
    }

    #[test]
    fn rect_gap_measures_horizontal_and_vertical_distance() {
        let a = Rect::new(0, 0, 100, 100);
        assert_eq!(rect_gap(a, Rect::new(130, 0, 50, 100)), 30); // 30px to the right
        assert_eq!(rect_gap(a, Rect::new(0, 150, 100, 50)), 50); // 50px below
        assert_eq!(rect_gap(a, Rect::new(130, 150, 50, 50)), 80); // diagonal: 30+50
    }
}
