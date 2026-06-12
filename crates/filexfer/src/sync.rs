//! Shared-folder synchronization planning.
//!
//! Given a local and a remote [`Manifest`] for a synced folder pair, [`plan`]
//! computes the set of [`SyncAction`]s needed to converge them — which files to
//! push, pull, or flag as conflicts. Execution is left to the transfer pump; this
//! module is pure logic and is the unit-tested heart of the feature.
//!
//! Resolution rules (per relative path):
//! * present on both, equal content → [`SyncAction::InSync`];
//! * present on both, different content → newer mtime wins ([`Push`]/[`Pull`]);
//!   equal mtime but different content → [`SyncAction::Conflict`];
//! * only local → [`Push`]; only remote → [`Pull`] (bidirectional only).
//!
//! [`Push`]: SyncAction::Push
//! [`Pull`]: SyncAction::Pull

use deskoryn_proto::{FileEntry, Manifest};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// Send the local copy to the peer.
    Push(String),
    /// Request the peer's copy.
    Pull(String),
    /// Both sides changed since they last agreed; needs user/policy resolution.
    Conflict(String),
    /// Already identical.
    InSync(String),
}

impl SyncAction {
    pub fn path(&self) -> &str {
        match self {
            SyncAction::Push(p) | SyncAction::Pull(p) | SyncAction::Conflict(p) | SyncAction::InSync(p) => p,
        }
    }
}

/// True if two entries have identical content (preferring hash, else size+mtime).
fn same_content(a: &FileEntry, b: &FileEntry) -> bool {
    match (a.hash, b.hash) {
        (Some(x), Some(y)) => x == y,
        _ => a.size == b.size && a.mtime == b.mtime,
    }
}

/// Compute the actions to converge `local` toward agreement with `remote`.
/// When `bidirectional` is false, remote-only files are not pulled (one-way
/// mirror from local to remote).
pub fn plan(local: &Manifest, remote: &Manifest, bidirectional: bool) -> Vec<SyncAction> {
    let rmap: HashMap<&str, &FileEntry> =
        remote.files.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e)).collect();
    let lmap: HashMap<&str, &FileEntry> =
        local.files.iter().filter(|e| !e.is_dir).map(|e| (e.rel_path.as_str(), e)).collect();

    let mut actions = Vec::new();

    for l in local.files.iter().filter(|e| !e.is_dir) {
        match rmap.get(l.rel_path.as_str()) {
            None => actions.push(SyncAction::Push(l.rel_path.clone())),
            Some(r) => {
                if same_content(l, r) {
                    actions.push(SyncAction::InSync(l.rel_path.clone()));
                } else {
                    actions.push(resolve(l, r));
                }
            }
        }
    }

    if bidirectional {
        for r in remote.files.iter().filter(|e| !e.is_dir) {
            if !lmap.contains_key(r.rel_path.as_str()) {
                actions.push(SyncAction::Pull(r.rel_path.clone()));
            }
        }
    }

    actions
}

fn resolve(local: &FileEntry, remote: &FileEntry) -> SyncAction {
    match (local.mtime, remote.mtime) {
        (Some(l), Some(r)) if l > r => SyncAction::Push(local.rel_path.clone()),
        (Some(l), Some(r)) if r > l => SyncAction::Pull(local.rel_path.clone()),
        // Equal (or unknown) timestamps but different content: can't safely pick.
        _ => SyncAction::Conflict(local.rel_path.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, mtime: u64, hash: Option<u8>) -> FileEntry {
        FileEntry {
            rel_path: path.into(),
            size,
            is_dir: false,
            mtime: Some(mtime),
            mode: None,
            hash: hash.map(|h| [h; 32]),
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        let total = files.iter().map(|f| f.size).sum();
        Manifest { files, total_bytes: total }
    }

    #[test]
    fn classifies_each_case() {
        let local = manifest(vec![
            entry("same.txt", 10, 100, Some(1)),
            entry("local_new.txt", 5, 200, Some(2)),   // newer locally -> push
            entry("remote_new.txt", 5, 100, Some(3)),  // older locally -> pull
            entry("only_local.txt", 7, 100, Some(4)),  // only here -> push
            entry("clash.txt", 9, 150, Some(5)),       // same mtime, diff hash -> conflict
        ]);
        let remote = manifest(vec![
            entry("same.txt", 10, 100, Some(1)),
            entry("local_new.txt", 5, 100, Some(9)),
            entry("remote_new.txt", 5, 200, Some(9)),
            entry("clash.txt", 9, 150, Some(6)),
            entry("only_remote.txt", 3, 100, Some(7)), // only there -> pull (bidi)
        ]);

        let actions = plan(&local, &remote, true);
        let has = |a: SyncAction| actions.contains(&a);
        assert!(has(SyncAction::InSync("same.txt".into())));
        assert!(has(SyncAction::Push("local_new.txt".into())));
        assert!(has(SyncAction::Pull("remote_new.txt".into())));
        assert!(has(SyncAction::Push("only_local.txt".into())));
        assert!(has(SyncAction::Conflict("clash.txt".into())));
        assert!(has(SyncAction::Pull("only_remote.txt".into())));
    }

    #[test]
    fn one_way_mirror_skips_remote_only() {
        let local = manifest(vec![entry("a.txt", 1, 100, Some(1))]);
        let remote = manifest(vec![entry("b.txt", 1, 100, Some(2))]);
        let actions = plan(&local, &remote, false);
        // Pushes a.txt; does NOT pull b.txt in one-way mode.
        assert!(actions.contains(&SyncAction::Push("a.txt".into())));
        assert!(!actions.iter().any(|a| matches!(a, SyncAction::Pull(_))));
    }
}
