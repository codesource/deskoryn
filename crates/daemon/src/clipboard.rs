//! Clipboard sync pump: bridge the local clipboard to the peer over the
//! Clipboard channel.
//!
//! Small text is inlined in the `Offer` (one round trip); other formats are
//! pulled on demand (delayed rendering). Receiving a write doesn't re-notify the
//! local watcher, so there is no A→B→A echo. See `docs/PROTOCOL.md`.

// Wired into the per-peer session (`crate::session`) over the Clipboard channel;
// the real OS backend is selected by `deskoryn_clipboard::platform::open_access`.

use bytes::BytesMut;
use deskoryn_clipboard::{ClipboardAccess, LocalClip};
use deskoryn_net::transport::{Session, Sink, Source};
use deskoryn_proto::{decode_one, encode, ClipFormat, ClipPayload, Clipboard, StreamPurpose};
use std::collections::HashMap;
use std::path::PathBuf;
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
/// `access` reads/writes the current local content. When `file_sync` is set,
/// file copies are advertised and (on `Pull`) streamed over a dedicated stream;
/// the *receiving* side is handled by [`crate::transfer::run_dispatcher`].
///
/// File-paste flow (the bytes ride a dedicated stream, not the clipboard): the
/// copier offers `FileList`; the pasting side `Pull`s; the copier streams the
/// remembered source paths via [`crate::transfer::send_files`] with a
/// `ClipboardFiles` purpose. Today the fetch is eager (on copy); true paste-time
/// deferral needs the OS render callback.
pub async fn run_clipboard(
    access: Arc<dyn ClipboardAccess>,
    mut local_changes: UnboundedReceiver<LocalClip>,
    mut sink: Box<dyn Sink>,
    mut source: Box<dyn Source>,
    inline_max: u64,
    session: Arc<dyn Session>,
    file_sync: bool,
) -> anyhow::Result<()> {
    // Source paths we've offered, keyed by the offer's seq, so a later
    // `Pull { FileList }` knows which files to stream.
    let mut offered_files: HashMap<u64, Vec<PathBuf>> = HashMap::new();

    loop {
        tokio::select! {
            change = local_changes.recv() => {
                let Some(change) = change else { return Ok(()); };
                let is_files = change.formats.first() == Some(&ClipFormat::FileList);
                if is_files {
                    // Remember the absolute source paths; advertise the list (the
                    // peer pulls, which triggers the transfer). Skipped if file
                    // sync is disabled or the backend can't read a file list.
                    if file_sync {
                        if let Some(paths) = access.read_files() {
                            if !paths.is_empty() {
                                offered_files.insert(change.seq, paths);
                                send(&mut sink, &Clipboard::Offer { seq: change.seq, formats: change.formats.clone(), inline: None }).await?;
                            }
                        }
                    }
                } else if let Some(&fmt) = change.formats.first() {
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
                        let fmt = formats.first().copied();
                        if fmt == Some(ClipFormat::FileList) {
                            // A file copy on the peer: ask them to send. The bytes
                            // arrive on a dedicated stream handled by the dispatcher.
                            if file_sync {
                                send(&mut sink, &Clipboard::Pull { seq, format: ClipFormat::FileList, tag: seq }).await?;
                            }
                        } else if let Some(fmt) = fmt {
                            send(&mut sink, &Clipboard::Pull { seq, format: fmt, tag: seq }).await?;
                        }
                    }
                    Clipboard::Pull { format: ClipFormat::FileList, seq, tag } => {
                        // The peer wants the files we offered under `seq`: stream
                        // their bytes over a dedicated `ClipboardFiles` stream.
                        if file_sync {
                            if let Some(paths) = offered_files.remove(&seq) {
                                spawn_file_send(session.clone(), tag, paths);
                            }
                        }
                    }
                    Clipboard::Pull { format, tag, .. } => {
                        if let Some(payload) = access.read(format) {
                            // Large payloads ride their own stream so they don't
                            // head-of-line-block the clipboard channel; small ones
                            // reply inline. (>16 MB still needs chunking — future.)
                            if payload_len(&payload) as u64 > inline_max {
                                spawn_clip_data_send(session.clone(), format, payload);
                            } else {
                                send(&mut sink, &Clipboard::Data { tag, payload }).await?;
                            }
                        }
                    }
                    Clipboard::Data { payload, .. } => access.write(payload),
                    Clipboard::DataStream { .. } => {
                        // Superseded by dedicated streams (StreamPurpose::ClipboardData,
                        // handled by the dispatcher); kept for wire compatibility.
                    }
                }
            }
        }
    }
}

/// Send a large pulled payload to the peer over a dedicated `ClipboardData`
/// stream (its purpose frame then one `Data` frame), so it doesn't block the
/// shared clipboard channel. The receiver's dispatcher writes it to the clipboard.
fn spawn_clip_data_send(session: Arc<dyn Session>, format: ClipFormat, payload: ClipPayload) {
    tokio::spawn(async move {
        let (mut sink, _source) = match session.open_stream().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "open_stream for clipboard data failed");
                return;
            }
        };
        let mut purpose = BytesMut::new();
        let mut data = BytesMut::new();
        if encode(&StreamPurpose::ClipboardData { format }, &mut purpose).is_err()
            || encode(&Clipboard::Data { tag: 0, payload }, &mut data).is_err()
        {
            tracing::warn!("clipboard payload too large to encode for a stream");
            return;
        }
        if let Err(e) = async { sink.send_bytes(&purpose).await?; sink.send_bytes(&data).await }.await {
            tracing::warn!(error = %e, "clipboard large-payload send failed");
        }
    });
}

/// Stream `paths` to the peer over a dedicated `ClipboardFiles` stream (sender
/// side of a file-paste). Spawned so the clipboard pump keeps syncing during the
/// transfer; the receiver's dispatcher lands them and updates its clipboard.
fn spawn_file_send(session: Arc<dyn Session>, tag: u64, paths: Vec<PathBuf>) {
    tokio::spawn(async move {
        let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
        if let Err(e) =
            crate::transfer::send_files(session.as_ref(), StreamPurpose::ClipboardFiles, tag, &refs)
                .await
        {
            tracing::warn!(error = %e, "clipboard file-send failed");
        }
    });
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
        let sa: Arc<dyn Session> = Arc::new(sa);
        let sb: Arc<dyn Session> = Arc::new(sb);
        let (tx_a, rx_src_a) = sa.channel(Channel::Clipboard).await.unwrap();
        let (tx_b, rx_src_b) = sb.channel(Channel::Clipboard).await.unwrap();

        let a: Arc<dyn ClipboardAccess> = acc_a;
        let b: Arc<dyn ClipboardAccess> = acc_b;
        let pa = tokio::spawn(run_clipboard(a, rx_a, tx_a, rx_src_a, 256 * 1024, sa.clone(), false));
        let pb = tokio::spawn(run_clipboard(b, rx_b, tx_b, rx_src_b, 256 * 1024, sb.clone(), false));

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn files_copy_paste_lands_in_download_dir() {
        use std::path::PathBuf;

        // A deterministic, test-local source file and download dir.
        let pid = std::process::id();
        let src = std::env::temp_dir().join(format!("deskoryn-clipsrc-{pid}"));
        let dl = std::env::temp_dir().join(format!("deskoryn-clipdl-{pid}"));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dl);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("note.txt"), b"clip files").unwrap();

        let (acc_a, inj_a, rx_a) = memory();
        let (acc_b, inj_b, rx_b) = memory();

        let (sa, sb) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let sa: Arc<dyn Session> = Arc::new(sa);
        let sb: Arc<dyn Session> = Arc::new(sb);
        let (tx_a, rx_src_a) = sa.channel(Channel::Clipboard).await.unwrap();
        let (tx_b, rx_src_b) = sb.channel(Channel::Clipboard).await.unwrap();

        let a: Arc<dyn ClipboardAccess> = acc_a;
        let b: Arc<dyn ClipboardAccess> = acc_b;
        let pa = tokio::spawn(run_clipboard(a, rx_a, tx_a, rx_src_a, 256 * 1024, sa.clone(), true));
        let pb = tokio::spawn(run_clipboard(b.clone(), rx_b, tx_b, rx_src_b, 256 * 1024, sb.clone(), true));
        // B's dispatcher accepts the incoming ClipboardFiles stream, lands the
        // files, and updates B's clipboard.
        let download = Some((dl.clone(), deskoryn_core::config::ConflictPolicy::Rename));
        let pd = tokio::spawn(crate::transfer::run_dispatcher(sb.clone(), Some(b), download));

        // Copy a file on A → it transfers over a dedicated stream and lands in B's
        // download dir, and B's clipboard receives the landed paths (ready to paste).
        inj_a.copy_files(vec![src.join("note.txt")]);

        let mut landed: Option<Vec<PathBuf>> = None;
        for _ in 0..300 {
            if let Some(l) = inj_b.landed_files() {
                if !l.is_empty() {
                    landed = Some(l);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let landed = landed.expect("file-paste did not land on B");
        assert_eq!(std::fs::read(&landed[0]).unwrap(), b"clip files");

        pa.abort();
        pb.abort();
        pd.abort();
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_payload_pulled_over_dedicated_stream() {
        let (acc_a, inj_a, rx_a) = memory();
        let (acc_b, inj_b, rx_b) = memory();

        let (sa, sb) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let sa: Arc<dyn Session> = Arc::new(sa);
        let sb: Arc<dyn Session> = Arc::new(sb);
        let (tx_a, rx_src_a) = sa.channel(Channel::Clipboard).await.unwrap();
        let (tx_b, rx_src_b) = sb.channel(Channel::Clipboard).await.unwrap();

        // inline_max = 1 KiB so our 64 KiB "image" must ride a dedicated stream.
        let a: Arc<dyn ClipboardAccess> = acc_a;
        let b: Arc<dyn ClipboardAccess> = acc_b;
        let pa = tokio::spawn(run_clipboard(a, rx_a, tx_a, rx_src_a, 1024, sa.clone(), false));
        let pb = tokio::spawn(run_clipboard(b.clone(), rx_b, tx_b, rx_src_b, 1024, sb.clone(), false));
        // B pulls; A opens a ClipboardData stream to send the payload; B's
        // dispatcher accepts it and writes it to B's clipboard.
        let pd = tokio::spawn(crate::transfer::run_dispatcher(sb.clone(), Some(b), None));

        let big = vec![0xABu8; 64 * 1024];
        inj_a.copy_image(big.clone());

        let mut ok = false;
        for _ in 0..300 {
            if let Some(ClipPayload::Bytes(got)) = inj_b.current() {
                assert_eq!(got, big);
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ok, "large image did not arrive on B via dedicated stream");

        pa.abort();
        pb.abort();
        pd.abort();
    }
}
