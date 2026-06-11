//! Building a [`Manifest`] by walking a set of source paths.

use deskoryn_proto::{FileEntry, Manifest};
use std::path::Path;

/// Walk `roots` and produce a manifest with normalized, forward-slashed relative
/// paths. Directories are emitted (so empty dirs are preserved) before their
/// contents. Hashing is deferred (filled in lazily as bytes are read) to keep
/// the offer instant for large trees.
///
/// This is a synchronous reference walker; the daemon runs it on a blocking task.
pub fn build(roots: &[&Path]) -> std::io::Result<Manifest> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for root in roots {
        let base = root.parent().unwrap_or(root);
        walk(root, base, &mut files, &mut total)?;
    }
    Ok(Manifest {
        files,
        total_bytes: total,
    })
}

fn walk(path: &Path, base: &Path, out: &mut Vec<FileEntry>, total: &mut u64) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    if meta.is_dir() {
        out.push(FileEntry {
            rel_path: rel,
            size: 0,
            is_dir: true,
            mtime,
            mode: unix_mode(&meta),
            hash: None,
        });
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        // Deterministic order helps resume and reproducible tests.
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            walk(&e.path(), base, out, total)?;
        }
    } else {
        *total += meta.len();
        out.push(FileEntry {
            rel_path: rel,
            size: meta.len(),
            is_dir: false,
            mtime,
            mode: unix_mode(&meta),
            hash: None, // computed during streaming
        });
    }
    Ok(())
}

#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.mode())
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}
