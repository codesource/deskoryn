//! Per-peer session driver.
//!
//! Owns one [`Session`] and the [`FocusMachine`], then pumps the logical channels
//! concurrently. This is where module wiring happens:
//!
//! * **Control**: handshake (`Hello`), heartbeat (`Ping`/`Pong` → drives
//!   reconnect detection), and `LayoutUpdate`.
//! * **Input**: when active, read captured events from `deskoryn-input`, run them
//!   through the [`FocusMachine`], inject locally or forward + hand off.
//! * **Clipboard / FileXfer / Audio**: bridge each module crate to its channel.
//!
//! The body below establishes the handshake and the structure; the per-channel
//! pumps are sketched with `TODO(impl)` where they call into the feature crates.

use crate::focus::{FocusAction, FocusMachine, Role};
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

    // The machine that hosts monitor index 0 of the anchor starts active; here
    // we simply start active if we contributed any monitors.
    let start_active = config.layout.monitors.iter().any(|m| m.device() == config.device.id);
    let mut focus = FocusMachine::new(
        config.device.id,
        layout,
        start_active,
        config.input.edge_resistance_px,
    );

    // --- Concurrent channel pumps -------------------------------------------
    //
    // In the full daemon these run as separate tasks joined here. The skeleton
    // shows the shape and demonstrates the focus loop with a couple of synthetic
    // motions so the dry-run does something observable.
    demo_focus_loop(&mut focus, &peer);

    // TODO(impl): spawn and join:
    //   - heartbeat: periodic Ping; mark the session dead if no Pong within N.
    //   - input pump: capture -> focus -> inject / forward (see below).
    //   - clipboard pump: ClipboardMonitor::next_change -> Offer; handle Pull.
    //   - filexfer pump: accept Offers, stream chunks, report Progress.
    //   - audio pump: capture -> Opus -> datagrams; datagrams -> jitter -> play.
    //
    // The input pump is the latency-critical one; sketch:
    //
    //   loop select {
    //     ev = capture.next_event() => match focus.on_motion(...) {
    //        MoveLocal(p)  => injector.warp_to(p),
    //        HandOff{to,entry,mods} => { send Input::Enter{entry,mods}; capture.set_grabbed(true) }
    //        ...
    //     },
    //     frame = input_rx.recv_bytes() => match decode {
    //        Input::Enter{entry,..} => { focus.on_enter(entry); capture.set_grabbed(false); injector.warp_to(entry) }
    //        Input::Events{events,..} => for e in events { injector.inject(e) }
    //     }
    //   }

    tracing::info!(%peer, role = ?focus.role(), "session ready (skeleton)");
    Ok(())
}

/// Drives a couple of motions through the focus machine so `--dry-run` produces
/// visible, deterministic output. Not part of the real loop.
fn demo_focus_loop(focus: &mut FocusMachine, peer: &deskoryn_core::DeviceId) {
    use deskoryn_core::input::Modifiers;
    if focus.role() != Role::Active {
        tracing::info!(%peer, "starting idle; awaiting Enter from peer");
        return;
    }
    for (dx, dy) in [(1000, 200), (5000, 0), (50, 0)] {
        match focus.on_motion(dx, dy, Modifiers::empty()) {
            FocusAction::MoveLocal(p) => tracing::debug!(?p, "move cursor locally"),
            FocusAction::HandOff { to, entry, .. } => {
                tracing::info!(%to, ?entry, "cursor crossed machine boundary — handing off")
            }
            other => tracing::debug!(?other, "focus action"),
        }
    }
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
