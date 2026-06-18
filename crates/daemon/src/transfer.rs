//! File-transfer pump: drive a [`FileXfer`] exchange over a session channel.
//!
//! The *logic* (manifest building, chunked writing, hashing, resume, conflict
//! handling, progress) lives in `deskoryn-filexfer`; this module wires it to the
//! wire protocol and the [`Session`] FileXfer channel.
//!
//! Two entry points:
//! * [`send_files`] — offer a set of paths and stream their bytes.
//! * [`receive`] — accept an offer and write the files to a download dir.

// Wired to the UI/drag-drop and shared-folder triggers in a later milestone;
// exercised today by the integration test below and usable from the session
// pump once those triggers land.
#![allow(dead_code)]

use bytes::BytesMut;
use deskoryn_clipboard::ClipboardAccess;
use deskoryn_core::config::ConflictPolicy;
use deskoryn_filexfer::manifest;
use deskoryn_filexfer::progress::ProgressTracker;
use deskoryn_filexfer::sink::FileSink;
use deskoryn_net::transport::{Session, Sink, Source};
use deskoryn_proto::{decode_one, encode, Clipboard, FileXfer, StreamPurpose, StreamTag};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Accept dedicated streams for the session's lifetime and route each by its
/// leading [`StreamPurpose`]: file transfers and clipboard file-pastes land in
/// `download` (the paste also updates the local file clipboard), and a large
/// clipboard payload is written straight to the clipboard. Each stream is
/// handled on its own task, so transfers run concurrently.
pub async fn run_dispatcher(
    session: Arc<dyn Session>,
    clipboard: Option<Arc<dyn ClipboardAccess>>,
    download: Option<(PathBuf, ConflictPolicy)>,
    max_concurrent: usize,
) -> anyhow::Result<()> {
    // Bound simultaneously-running transfers: each handler holds a permit for its
    // lifetime, so a peer opening a burst of streams queues rather than spawning
    // unbounded tasks. `max(1)` guards against a zero from config.
    let limit = Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    loop {
        let Some((sink, mut source)) = session.accept_stream().await? else {
            return Ok(());
        };
        let purpose = read_purpose(&mut source).await?;
        // Acquire after reading the (cheap) purpose; if we're at the cap this
        // awaits — applying backpressure to further accepts too.
        let permit = limit.clone().acquire_owned().await.expect("semaphore not closed");
        match purpose {
            Some(StreamPurpose::FileTransfer) => {
                let Some((dir, policy)) = download.clone() else { continue };
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = receive_on_stream(sink, source, &dir, policy).await {
                        tracing::warn!(error = %e, "file transfer receive failed");
                    }
                });
            }
            Some(StreamPurpose::ClipboardFiles) => {
                let (Some((dir, policy)), Some(clip)) = (download.clone(), clipboard.clone())
                else {
                    continue;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    match receive_on_stream(sink, source, &dir, policy).await {
                        Ok(landed) => {
                            tracing::info!(count = landed.len(), "clipboard file-paste received");
                            clip.write_files(&landed);
                        }
                        Err(e) => tracing::warn!(error = %e, "clipboard file-receive failed"),
                    }
                });
            }
            Some(StreamPurpose::ClipboardData { .. }) => {
                let Some(clip) = clipboard.clone() else { continue };
                tokio::spawn(async move {
                    let _permit = permit;
                    // A large clipboard payload: a single Data frame follows.
                    if let Ok(Some(frame)) = source.recv_bytes().await {
                        let mut b = BytesMut::from(&frame[..]);
                        if let Ok(Some(Clipboard::Data { payload, .. })) = decode_one::<Clipboard>(&mut b) {
                            clip.write(payload);
                        }
                    }
                });
            }
            None => {} // stream closed before announcing its purpose
        }
    }
}

/// Bytes per `Chunk`. 64 KiB balances framing overhead against memory.
const CHUNK: usize = 64 * 1024;

async fn send_msg(sink: &mut Box<dyn Sink>, msg: &FileXfer) -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    encode(msg, &mut buf)?;
    sink.send_bytes(&buf).await?;
    Ok(())
}

/// Write the leading [`StreamPurpose`] frame that tells the peer's
/// `accept_stream` dispatcher how to route this dedicated stream.
async fn send_purpose(sink: &mut Box<dyn Sink>, purpose: StreamPurpose) -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    encode(&purpose, &mut buf)?;
    sink.send_bytes(&buf).await?;
    Ok(())
}

/// Read that leading [`StreamPurpose`] frame off a freshly accepted stream.
pub async fn read_purpose(source: &mut Box<dyn Source>) -> anyhow::Result<Option<StreamPurpose>> {
    match source.recv_bytes().await? {
        Some(frame) => {
            let mut b = BytesMut::from(&frame[..]);
            Ok(decode_one::<StreamPurpose>(&mut b)?)
        }
        None => Ok(None),
    }
}

async fn recv_msg(source: &mut Box<dyn Source>) -> anyhow::Result<Option<FileXfer>> {
    match source.recv_bytes().await? {
        Some(frame) => {
            let mut b = BytesMut::from(&frame[..]);
            Ok(decode_one::<FileXfer>(&mut b)?)
        }
        None => Ok(None),
    }
}

/// Offer `roots` to the peer and stream their contents over a **dedicated
/// stream** (so concurrent transfers never head-of-line-block each other).
/// `purpose` is the leading frame the peer routes on. Returns when the peer has
/// acknowledged the whole set (or rejected it).
pub async fn send_files(
    session: &dyn Session,
    purpose: StreamPurpose,
    tag: StreamTag,
    roots: &[&Path],
) -> anyhow::Result<()> {
    let manifest = tokio::task::block_in_place(|| manifest::build(roots))?;
    let (mut sink, mut source) = session.open_stream().await?;
    send_purpose(&mut sink, purpose).await?;

    send_msg(&mut sink, &FileXfer::Offer { tag, manifest: manifest.clone() }).await?;

    // Await Accept / Reject.
    match recv_msg(&mut source).await? {
        Some(FileXfer::Accept { resume, .. }) => {
            stream_files(&mut sink, tag, &manifest, &resume, roots).await?;
            send_msg(&mut sink, &FileXfer::Complete { tag }).await?;
            // Wait for the receiver's completion ack so the caller doesn't close
            // the connection before the reliable stream has actually delivered.
            let _ = recv_msg(&mut source).await?;
            Ok(())
        }
        Some(FileXfer::Reject { reason, .. }) => {
            anyhow::bail!("peer rejected transfer: {reason}")
        }
        other => anyhow::bail!("unexpected reply to offer: {other:?}"),
    }
}

async fn stream_files(
    sink: &mut Box<dyn Sink>,
    tag: StreamTag,
    manifest: &deskoryn_proto::Manifest,
    resume: &[deskoryn_proto::FileResume],
    roots: &[&Path],
) -> anyhow::Result<()> {
    // The manifest's rel_paths are relative to each root's parent; reconstruct
    // the absolute source path the same way `manifest::build` did.
    let base = roots.first().and_then(|r| r.parent()).unwrap_or(Path::new("."));
    for (idx, entry) in manifest.files.iter().enumerate() {
        if entry.is_dir {
            continue;
        }
        let abs = base.join(&entry.rel_path);
        let start = resume
            .iter()
            .find(|r| r.file_index as usize == idx)
            .map(|r| r.offset)
            .unwrap_or(0);

        let bytes = tokio::fs::read(&abs).await?;
        let mut offset = start as usize;
        while offset < bytes.len() {
            let end = (offset + CHUNK).min(bytes.len());
            send_msg(
                sink,
                &FileXfer::Chunk {
                    tag,
                    file_index: idx as u32,
                    offset: offset as u64,
                    data: bytes[offset..end].to_vec(),
                },
            )
            .await?;
            offset = end;
        }
    }
    Ok(())
}

/// Accept an offer that has already arrived on a dedicated stream (its
/// [`StreamPurpose`] consumed by the dispatcher) and write the files under
/// `download_dir`. Returns the paths written.
pub async fn receive_on_stream(
    mut sink: Box<dyn Sink>,
    mut source: Box<dyn Source>,
    download_dir: &Path,
    policy: ConflictPolicy,
) -> anyhow::Result<Vec<PathBuf>> {
    let manifest = match recv_msg(&mut source).await? {
        Some(FileXfer::Offer { tag, manifest }) => {
            // Accept everything from the start (no resume yet).
            send_msg(&mut sink, &FileXfer::Accept { tag, resume: vec![] }).await?;
            (tag, manifest)
        }
        other => anyhow::bail!("expected an Offer, got {other:?}"),
    };
    let (tag, manifest) = manifest;

    tokio::fs::create_dir_all(download_dir).await?;

    // Resolve destinations up front (creating directories), so chunks can be
    // routed by file_index. `dests[i]` is None for directories / skipped files.
    let mut dests: Vec<Option<PathBuf>> = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        let intended = download_dir.join(&entry.rel_path);
        if entry.is_dir {
            tokio::fs::create_dir_all(&intended).await?;
            dests.push(None);
            continue;
        }
        let resolved = deskoryn_filexfer::resolve_conflict(download_dir, &intended, policy)?;
        dests.push(resolved);
    }

    let mut sinks: Vec<Option<FileSink>> = (0..manifest.files.len()).map(|_| None).collect();
    let mut tracker = ProgressTracker::new(0, manifest.total_bytes, manifest.files.len() as u32);
    let mut written = Vec::new();

    loop {
        match recv_msg(&mut source).await? {
            Some(FileXfer::Chunk { file_index, offset, data, .. }) => {
                let i = file_index as usize;
                let Some(Some(dest)) = dests.get(i).cloned() else {
                    continue; // directory or skipped
                };
                if sinks[i].is_none() {
                    sinks[i] = Some(FileSink::create(&dest, 0)?);
                }
                let fs = sinks[i].as_mut().unwrap();
                fs.write_chunk(offset, &data)?;
                tracker.advance(data.len() as u64);
            }
            Some(FileXfer::Complete { .. }) => break,
            Some(FileXfer::Cancel { reason, .. }) => anyhow::bail!("transfer cancelled: {reason}"),
            Some(other) => tracing::debug!(?other, "unexpected file-transfer message"),
            None => anyhow::bail!("peer closed mid-transfer"),
        }
    }

    // Finalize each opened file (verifying hash where the manifest had one).
    for (i, slot) in sinks.into_iter().enumerate() {
        if let Some(fs) = slot {
            let entry = &manifest.files[i];
            fs.finish(entry.hash, &entry.rel_path)?;
            tracker.complete_file();
            if let Some(Some(dest)) = dests.get(i) {
                written.push(dest.clone());
            }
        }
    }

    // Acknowledge completion so the sender can close cleanly.
    send_msg(&mut sink, &FileXfer::Complete { tag }).await?;
    Ok(written)
}

// --- User-facing one-shot commands -----------------------------------------

/// `deskorynd send <addr> <files...>` — connect to a paired peer and send files.
pub async fn send_command(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    paths: deskoryn_core::config::Paths,
    addr: String,
    files: Vec<PathBuf>,
) -> anyhow::Result<()> {
    #[cfg(any(feature = "linux", feature = "windows"))]
    {
        send_impl(config, paths, addr, files).await
    }
    #[cfg(not(any(feature = "linux", feature = "windows")))]
    {
        let _ = (config, paths, addr, files);
        anyhow::bail!("file transfer requires a real build: rebuild with `--features linux` (or `windows`)")
    }
}

/// `deskorynd receive` — accept one incoming transfer from a paired peer.
pub async fn receive_command(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    paths: deskoryn_core::config::Paths,
    dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    #[cfg(any(feature = "linux", feature = "windows"))]
    {
        receive_impl(config, paths, dir).await
    }
    #[cfg(not(any(feature = "linux", feature = "windows")))]
    {
        let _ = (config, paths, dir);
        anyhow::bail!("file transfer requires a real build: rebuild with `--features linux` (or `windows`)")
    }
}

#[cfg(any(feature = "linux", feature = "windows"))]
async fn send_impl(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    paths: deskoryn_core::config::Paths,
    addr: String,
    files: Vec<PathBuf>,
) -> anyhow::Result<()> {
    use deskoryn_core::trust::TrustStore;
    use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let identity = Arc::new(DeviceIdentity::load_or_generate(
        config.device.id,
        &paths.cert_file(),
        &paths.key_file(),
    )?);
    let trust = Arc::new(Mutex::new(TrustStore::load(&paths.trust_file())?));
    let endpoint = QuicEndpoint::bind(0, identity, trust).await?;

    let socket = addr.parse().map_err(|e| anyhow::anyhow!("invalid address: {e}"))?;
    println!("Connecting to {addr} …");
    let session = endpoint.connect_any(socket).await?;
    let refs: Vec<&Path> = files.iter().map(|p| p.as_path()).collect();
    println!("Sending {} item(s) …", refs.len());
    send_files(session.as_ref(), StreamPurpose::FileTransfer, 1, &refs).await?;
    session.close("transfer complete").await;
    println!("Done.");
    Ok(())
}

#[cfg(any(feature = "linux", feature = "windows"))]
async fn receive_impl(
    config: std::sync::Arc<deskoryn_core::config::AppConfig>,
    paths: deskoryn_core::config::Paths,
    dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    use deskoryn_core::trust::TrustStore;
    use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let identity = Arc::new(DeviceIdentity::load_or_generate(
        config.device.id,
        &paths.cert_file(),
        &paths.key_file(),
    )?);
    let trust = Arc::new(Mutex::new(TrustStore::load(&paths.trust_file())?));
    let endpoint = QuicEndpoint::bind(config.network.listen_port, identity, trust).await?;

    let download = dir
        .or_else(|| config.file_transfer.download_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("deskoryn-received"));
    println!(
        "Waiting for a transfer on port {} → {}",
        endpoint.local_port(),
        download.display()
    );
    let session = endpoint.accept().await?;
    let (sink, mut source) = session
        .accept_stream()
        .await?
        .ok_or_else(|| anyhow::anyhow!("peer closed before opening a transfer stream"))?;
    let purpose = read_purpose(&mut source).await?;
    tracing::debug!(?purpose, "incoming transfer stream");
    let got =
        receive_on_stream(sink, source, &download, config.file_transfer.conflict_policy).await?;
    println!("Received {} file(s):", got.len());
    for p in &got {
        println!("  {}", p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_core::DeviceId;
    use deskoryn_net::transport::loopback;
    use std::io::Write;
    use std::time::Duration;

    fn tmpdir(tag: &str) -> PathBuf {
        // Deterministic, test-local temp dir (no clock/random needed).
        let base = std::env::temp_dir().join(format!("deskoryn-xfer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transfers_a_directory_tree() {
        // Build a source tree: src/a.txt, src/sub/b.bin
        let root = tmpdir("src");
        let src = root.join("payload");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello deskoryn").unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(src.join("sub/b.bin")).unwrap().write_all(&big).unwrap();

        let dl = tmpdir("dl");

        let (sa, sb) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let sa: Box<dyn Session> = Box::new(sa);
        let sb: Box<dyn Session> = Box::new(sb);

        let src_for_send = src.clone();
        let sender = tokio::spawn(async move {
            send_files(sa.as_ref(), StreamPurpose::FileTransfer, 1, &[src_for_send.as_path()]).await
        });

        // Receiver: accept the dedicated stream, read its purpose, then receive.
        let (sink, mut source) = sb.accept_stream().await.unwrap().unwrap();
        let purpose = read_purpose(&mut source).await.unwrap();
        assert_eq!(purpose, Some(StreamPurpose::FileTransfer));
        let dl_for_recv = dl.clone();
        let received = receive_on_stream(sink, source, &dl_for_recv, ConflictPolicy::Rename)
            .await
            .unwrap();
        sender.await.unwrap().unwrap();

        // Two files received with identical contents.
        assert_eq!(received.len(), 2);
        let got_a = std::fs::read(dl.join("payload/a.txt")).unwrap();
        assert_eq!(got_a, b"hello deskoryn");
        let got_b = std::fs::read(dl.join("payload/sub/b.bin")).unwrap();
        assert_eq!(got_b, big);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatcher_delivers_all_transfers_under_concurrency_cap() {
        // More concurrent transfers than the cap → they queue, but all deliver.
        let root = tmpdir("multisrc");
        let n = 6usize;
        let mut files = Vec::new();
        for i in 0..n {
            let p = root.join(format!("f{i}.txt"));
            std::fs::write(&p, format!("payload-{i}")).unwrap();
            files.push(p);
        }
        let dl = tmpdir("multidl");

        let (sa, sb) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let sa: Arc<dyn Session> = Arc::new(sa);
        let sb: Arc<dyn Session> = Arc::new(sb);

        // Receiver dispatcher capped at 2 concurrent transfers.
        let disp = tokio::spawn(run_dispatcher(
            sb.clone(),
            None,
            Some((dl.clone(), ConflictPolicy::Rename)),
            2,
        ));

        // Fire all N transfers at once, each on its own dedicated stream.
        let mut senders = Vec::new();
        for f in &files {
            let sa2 = sa.clone();
            let f2 = f.clone();
            senders.push(tokio::spawn(async move {
                send_files(sa2.as_ref(), StreamPurpose::FileTransfer, 1, &[f2.as_path()]).await
            }));
        }
        for s in senders {
            s.await.unwrap().unwrap();
        }

        // Every file lands despite the cap (queued, not dropped).
        for i in 0..n {
            let want = dl.join(format!("f{i}.txt"));
            let mut ok = false;
            for _ in 0..300 {
                if want.exists() && std::fs::read(&want).unwrap() == format!("payload-{i}").as_bytes() {
                    ok = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(ok, "f{i}.txt did not arrive under the concurrency cap");
        }

        disp.abort();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dl);
    }
}
