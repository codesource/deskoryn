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
use deskoryn_net::transport::Session;
use deskoryn_proto::{Channel, Control, PROTOCOL_VERSION};
use std::sync::Arc;

pub async fn run(config: Arc<AppConfig>, session: Box<dyn Session>) -> anyhow::Result<()> {
    let peer = session.peer();
    tracing::info!(%peer, "session established");

    // --- Handshake on the Control channel -----------------------------------
    let (mut ctl_tx, mut ctl_rx) = session.channel(Channel::Control).await?;
    let mut buf = bytes::BytesMut::new();
    let hello = Control::Hello {
        version: PROTOCOL_VERSION,
        device: config.device.id,
        name: config.device.name.clone(),
        monitors: config.layout.clone(),
        capabilities: capabilities_from(&config),
    };
    deskoryn_proto::encode(&hello, &mut buf)?;
    ctl_tx.send_bytes(&buf).await?;

    // Read the peer's Hello and merge layouts.
    let mut layout = config.layout.clone();
    if let Some(frame) = ctl_rx.recv_bytes().await? {
        let mut b = bytes::BytesMut::from(&frame[..]);
        if let Some(Control::Hello { name, monitors, version, .. }) =
            deskoryn_proto::decode_one::<Control>(&mut b)?
        {
            tracing::info!(peer_name = %name, ?version, "peer hello");
            // The combined virtual desktop is the union of both monitor sets.
            layout.monitors.extend(monitors.monitors);
        }
    }

    // Build the input controller over the combined virtual desktop, starting the
    // cursor on one of our own monitors.
    let start = start_position(&layout, config.device.id);
    let controller = crate::input::Controller::new(layout, config.device.id, start)
        .with_input_config(&config.input);
    let capture = deskoryn_input::platform::open_capture()?;
    let injector = deskoryn_input::platform::open_injector()?;
    tracing::info!(backend = ?deskoryn_input::platform::detect(), "input backend");

    // --- Concurrent channel pumps -------------------------------------------
    //
    // Run the control pump (heartbeat + control messages) and the input pump
    // (capture -> forward / inject) for the lifetime of the session; whichever
    // ends first tears the session down so the supervisor can reconnect.
    tracing::info!(%peer, "session ready; starting pumps");
    let control = crate::control::run_control(ctl_tx, ctl_rx, crate::control::HeartbeatConfig::default());
    let input = crate::input::run_input(session.as_ref(), controller, capture, injector);

    let end = tokio::select! {
        r = control => { r.map(|e| format!("{e:?}")) }
        r = input => { r.map(|_| "input pump ended".to_string()) }
    }?;
    tracing::info!(%peer, %end, "session ended");
    Ok(())
}

/// Center of the first monitor this device owns (or the desktop origin).
fn start_position(layout: &deskoryn_core::VirtualDesktop, me: deskoryn_core::DeviceId) -> deskoryn_core::geometry::Point {
    layout
        .monitors
        .iter()
        .find(|m| m.device() == me)
        .map(|m| deskoryn_core::geometry::Point::new(m.bounds.x + m.bounds.w / 2, m.bounds.y + m.bounds.h / 2))
        .unwrap_or(deskoryn_core::geometry::Point::new(0, 0))
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
