//! Clipboard sync pump: bridge the local clipboard to the peer over the
//! Clipboard channel.
//!
//! Small text is inlined in the `Offer` (one round trip); other formats are
//! pulled on demand (delayed rendering). Receiving a write doesn't re-notify the
//! local watcher, so there is no A→B→A echo. See `docs/PROTOCOL.md`.

// Wired to live OS clipboards in the M3 backends; exercised today by the
// integration test and runnable over any session.
#![allow(dead_code)]

use bytes::BytesMut;
use deskoryn_clipboard::{ClipboardAccess, LocalClip};
use deskoryn_net::transport::{Sink, Source};
use deskoryn_proto::{decode_one, encode, ClipPayload, Clipboard};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

fn payload_len(p: &ClipPayload) -> usize {
    match p {
        ClipPayload::Text(s) | ClipPayload::Html(s) => s.len(),
        ClipPayload::Bytes(b) => b.len(),
        ClipPayload::Files(_) => usize::MAX, // never inline a file list
    }
}

async fn send(sink: &mut Box<dyn Sink>, msg: &Clipboard) -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    encode(msg, &mut buf)?;
    sink.send_bytes(&buf).await?;
    Ok(())
}

/// Run the clipboard pump until either side closes.
///
/// `local_changes` yields a [`LocalClip`] whenever the local clipboard changes;
/// `access` reads/writes the current local content.
pub async fn run_clipboard(
    access: Arc<dyn ClipboardAccess>,
    mut local_changes: UnboundedReceiver<LocalClip>,
    mut sink: Box<dyn Sink>,
    mut source: Box<dyn Source>,
    inline_max: u64,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            change = local_changes.recv() => {
                let Some(change) = change else { return Ok(()); };
                if let Some(&fmt) = change.formats.first() {
                    if let Some(payload) = access.read(fmt) {
                        let inline = (payload_len(&payload) as u64 <= inline_max).then(|| payload.clone());
                        send(&mut sink, &Clipboard::Offer { seq: change.seq, formats: change.formats.clone(), inline }).await?;
                    }
                }
            }
            frame = source.recv_bytes() => {
                let Some(frame) = frame? else { return Ok(()); };
                let mut b = BytesMut::from(&frame[..]);
                let Some(msg) = decode_one::<Clipboard>(&mut b)? else { continue; };
                match msg {
                    Clipboard::Offer { inline: Some(payload), .. } => access.write(payload),
                    Clipboard::Offer { seq, formats, inline: None } => {
                        if let Some(&fmt) = formats.first() {
                            send(&mut sink, &Clipboard::Pull { seq, format: fmt, tag: seq }).await?;
                        }
                    }
                    Clipboard::Pull { format, tag, .. } => {
                        if let Some(payload) = access.read(format) {
                            send(&mut sink, &Clipboard::Data { tag, payload }).await?;
                        }
                    }
                    Clipboard::Data { payload, .. } => access.write(payload),
                    Clipboard::DataStream { .. } => {
                        // TODO(impl): pull a large payload over a dedicated stream.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_clipboard::platform::memory;
    use deskoryn_core::DeviceId;
    use deskoryn_net::transport::{loopback, Session};
    use deskoryn_proto::Channel;
    use std::time::Duration;

    async fn wait_text(inj: &deskoryn_clipboard::platform::ClipInjector, want: &str) -> bool {
        for _ in 0..100 {
            if let Some(ClipPayload::Text(t)) = inj.current() {
                if t == want {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn text_syncs_both_directions() {
        let (acc_a, inj_a, rx_a) = memory();
        let (acc_b, inj_b, rx_b) = memory();

        let (sa, sb) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let (tx_a, rx_src_a) = sa.channel(Channel::Clipboard).await.unwrap();
        let (tx_b, rx_src_b) = sb.channel(Channel::Clipboard).await.unwrap();

        let a: Arc<dyn ClipboardAccess> = acc_a;
        let b: Arc<dyn ClipboardAccess> = acc_b;
        let pa = tokio::spawn(run_clipboard(a, rx_a, tx_a, rx_src_a, 256 * 1024));
        let pb = tokio::spawn(run_clipboard(b, rx_b, tx_b, rx_src_b, 256 * 1024));

        // Copy on A → appears on B.
        inj_a.copy_text("hello from A");
        assert!(wait_text(&inj_b, "hello from A").await, "A→B text sync failed");

        // Copy on B → appears on A.
        inj_b.copy_text("reply from B");
        assert!(wait_text(&inj_a, "reply from B").await, "B→A text sync failed");

        // Sanity: the read path exposes the synced format.
        assert!(matches!(inj_b.current(), Some(ClipPayload::Text(_))));

        pa.abort();
        pb.abort();
    }
}
