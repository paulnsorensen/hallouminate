use std::collections::HashMap;

use crate::common::{FileRef, Mtime};
use crate::corpus::blake3_file;

/// Storage-agnostic view of a previously-indexed file. Built from the
/// denormalized columns of the chunks table by `LanceStore::list_files`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    /// Stored path key of the file, as written to the chunks table.
    pub file_ref: String,
    /// Corpus the file belongs to.
    pub corpus: String,
    /// Last-modified time recorded at the previous index, in epoch millis.
    pub mtime_ms: i64,
    /// blake3 hex digest of the file's content at the previous index.
    pub content_hash: String,
}

/// The diff between what is on disk and what is already indexed, split into the
/// three actions [`apply`](crate::indexer::apply) performs: write new/changed
/// files, fast-path mtime bumps, and delete vanished files.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexPlan {
    /// Files absent from the index — written in full (chunked + embedded).
    pub upserts: Vec<Upsert>,
    /// Files whose mtime moved while their snapshot stayed. The mtime change
    /// is only a hint that content *may* have changed: `apply` re-hashes each
    /// candidate and, when the hash matches the snapshot, takes the fast path
    /// of bumping the stored mtime without re-chunking or re-embedding.
    /// Candidates whose hash differs fall through to the upsert path.
    pub mtime_touches: Vec<MtimeCandidate>,
    /// Snapshots of files gone from disk — deleted from the index.
    pub deletes: Vec<FileSnapshot>,
}

/// A file to (re)index in full because no prior snapshot exists for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upsert {
    /// Path of the file on disk.
    pub file: FileRef,
    /// Current mtime to record for the file.
    pub mtime: Mtime,
}

/// A file whose mtime moved since the last index. Carries the prior `snap` so
/// `apply` can compare content hashes and decide between the fast mtime-only
/// path and a full re-index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtimeCandidate {
    /// Path of the file on disk.
    pub file: FileRef,
    /// Snapshot from the previous index, including the old content hash.
    pub snap: FileSnapshot,
    /// Current on-disk mtime to record if the fast path is taken.
    pub new_mtime: Mtime,
    /// Content hash already computed by the caller (e.g. the single-file
    /// reroute's same-mtime hash check), so `apply` can skip re-hashing the
    /// file. `None` when no hash has been computed yet (the bulk `plan()`
    /// path below).
    pub known_hash: Option<String>,
}

pub fn plan(disk: Vec<(FileRef, Mtime)>, mut db: HashMap<FileRef, FileSnapshot>) -> IndexPlan {
    // Dedup by FileRef: overlapping CorpusConfig.paths can yield the same file
    // twice. Last occurrence wins (preserves the most recent mtime observation).
    let disk: HashMap<FileRef, Mtime> = disk.into_iter().collect();
    let mut out = IndexPlan::default();
    for (file, mtime) in disk {
        match db.remove(&file) {
            None => out.upserts.push(Upsert { file, mtime }),
            Some(snap) if snap.mtime_ms == mtime.0 => {
                // Millisecond mtime resolution can hide a real content change
                // (same-mtime edit, clock skew): verify with a content hash
                // before declaring the file unchanged, mirroring the
                // single-file reroute's hash check in dispatch.rs. A hash
                // read failure means the file is transiently unreadable —
                // log and keep the existing snapshot (pre-hash-verify
                // behavior), rather than routing to apply() where its own
                // re-hash would abort the whole corpus index.
                match blake3_file(file.as_path()) {
                    Ok(hash) if hash == snap.content_hash => continue,
                    Ok(hash) => out.mtime_touches.push(MtimeCandidate {
                        file,
                        snap,
                        new_mtime: mtime,
                        known_hash: Some(hash),
                    }),
                    Err(e) => {
                        tracing::warn!(
                            target: "hallouminate::indexer",
                            file = %file.as_path().display(),
                            error = %e,
                            "skipping hash verification: file unreadable, keeping existing snapshot"
                        );
                    }
                }
            }
            Some(snap) => out.mtime_touches.push(MtimeCandidate {
                file,
                snap,
                new_mtime: mtime,
                known_hash: None,
            }),
        }
    }
    let mut leftover: Vec<FileSnapshot> = db.into_values().collect();
    leftover.sort_by(|a, b| a.file_ref.cmp(&b.file_ref));
    out.deletes = leftover;
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fref(path: &str) -> FileRef {
        FileRef::new(PathBuf::from(path))
    }

    fn snap(file_ref: &str, mtime_ms: i64, hash: &str) -> FileSnapshot {
        FileSnapshot {
            file_ref: file_ref.to_string(),
            corpus: "docs".to_string(),
            mtime_ms,
            content_hash: hash.to_string(),
        }
    }

    /// Writes `content` to a real file so `plan()`'s hash-verification I/O
    /// has something to read; returns the path and its blake3 hash.
    fn write_file(dir: &std::path::Path, name: &str, content: &[u8]) -> (PathBuf, String) {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write fixture file");
        let hash = blake3_file(&path).expect("hash fixture file");
        (path, hash)
    }

    #[test]
    fn plan_routes_new_file_into_upserts() {
        let disk = vec![(fref("/tmp/new.md"), Mtime(42))];
        let db = HashMap::new();
        let p = plan(disk, db);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].file, fref("/tmp/new.md"));
        assert_eq!(p.upserts[0].mtime, Mtime(42));
        assert!(p.mtime_touches.is_empty());
        assert!(p.deletes.is_empty());
    }

    #[test]
    fn plan_skips_files_with_unchanged_mtime_and_matching_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, hash) = write_file(dir.path(), "stable.md", b"stable content");
        let file = FileRef::new(path.clone());
        let disk = vec![(file.clone(), Mtime(100))];
        let mut db = HashMap::new();
        db.insert(file, snap(path.to_str().unwrap(), 100, &hash));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.mtime_touches.is_empty());
        assert!(p.deletes.is_empty());
    }

    #[test]
    fn plan_reindexes_when_content_hash_differs_despite_unchanged_mtime() {
        // Regression for the bulk planner trusting mtime alone: a file whose
        // millisecond mtime happens to be unchanged (clock resolution,
        // same-mtime overwrite) but whose content hash moved must NOT be
        // silently skipped — it must be routed for a hash-verified re-index,
        // matching the single-file reroute's same-mtime hash check.
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, real_hash) = write_file(dir.path(), "changed-in-place.md", b"new content");
        let file = FileRef::new(path.clone());
        let disk = vec![(file.clone(), Mtime(100))];
        let mut db = HashMap::new();
        db.insert(
            file.clone(),
            snap(path.to_str().unwrap(), 100, "stale-hash-from-old-content"),
        );
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.deletes.is_empty());
        assert_eq!(p.mtime_touches.len(), 1);
        let cand = &p.mtime_touches[0];
        assert_eq!(cand.file, file);
        assert_eq!(cand.new_mtime, Mtime(100));
        assert_eq!(cand.snap.content_hash, "stale-hash-from-old-content");
        assert_eq!(
            cand.known_hash,
            Some(real_hash),
            "plan() must carry the hash it already computed so apply() doesn't re-hash the file for the touch-vs-upsert decision"
        );
    }

    #[test]
    fn plan_routes_mtime_change_into_touch_candidates() {
        let file = fref("/tmp/changed.md");
        let disk = vec![(file.clone(), Mtime(200))];
        let mut db = HashMap::new();
        db.insert(file.clone(), snap("/tmp/changed.md", 100, "cafebabe"));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.deletes.is_empty());
        assert_eq!(p.mtime_touches.len(), 1);
        let cand = &p.mtime_touches[0];
        assert_eq!(cand.file, file);
        assert_eq!(cand.new_mtime, Mtime(200));
        assert_eq!(cand.snap.mtime_ms, 100);
        assert_eq!(cand.snap.content_hash, "cafebabe");
        assert_eq!(cand.known_hash, None);
    }

    #[test]
    fn plan_routes_vanished_files_into_deletes() {
        let disk: Vec<(FileRef, Mtime)> = Vec::new();
        let mut db = HashMap::new();
        db.insert(fref("/tmp/gone.md"), snap("/tmp/gone.md", 50, "f00d"));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.mtime_touches.is_empty());
        assert_eq!(p.deletes.len(), 1);
        assert_eq!(p.deletes[0].file_ref, "/tmp/gone.md");
    }

    #[cfg(unix)]
    fn nix_getuid_is_zero() -> bool {
        // Avoid a libc dep just for this; read /proc/self/status on Linux,
        // shell out to `id -u` everywhere else (macOS, BSDs). The test
        // tolerates either path failing — worst case we run the assertion
        // when we shouldn't, which only false-positives in CI containers
        // running as root, where the assertion is a no-op anyway.
        if let Ok(s) = std::fs::read_to_string("/proc/self/status")
            && let Some(line) = s.lines().find(|l| l.starts_with("Uid:"))
        {
            return line.split_whitespace().nth(1) == Some("0");
        }
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn plan_keeps_existing_snapshot_when_same_mtime_file_is_unreadable() {
        // Regression: a same-mtime, already-indexed file that is
        // transiently unreadable during the hash-verify check must be
        // planned as unchanged, not routed to mtime_touches — routing it
        // would hit apply()'s own re-hash and abort the whole corpus index
        // (pre-hash-verify behavior was a zero-IO silent skip).
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, hash) = write_file(dir.path(), "locked.md", b"stable content");
        let is_root = nix_getuid_is_zero();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let file = FileRef::new(path.clone());
        let disk = vec![(file.clone(), Mtime(100))];
        let mut db = HashMap::new();
        db.insert(file, snap(path.to_str().unwrap(), 100, &hash));
        let p = plan(disk, db);
        // Restore perms before any potential assertion failure unwind, so
        // the tempdir can be cleaned up.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        if is_root {
            return; // root reads through 0o000; the negative test is meaningless.
        }
        assert!(p.upserts.is_empty());
        assert!(
            p.mtime_touches.is_empty(),
            "unreadable same-mtime file must not route to apply(); got {:?}",
            p.mtime_touches
        );
        assert!(p.deletes.is_empty());
    }

    #[test]
    fn plan_handles_full_matrix_simultaneously() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (stable_path, stable_hash) = write_file(dir.path(), "stable.md", b"stable content");

        let new = fref("/tmp/new.md");
        let stable = FileRef::new(stable_path.clone());
        let changed = fref("/tmp/changed.md");
        let gone = fref("/tmp/gone.md");
        let disk = vec![
            (new.clone(), Mtime(1)),
            (stable.clone(), Mtime(2)),
            (changed.clone(), Mtime(30)),
        ];
        let mut db = HashMap::new();
        db.insert(stable, snap(stable_path.to_str().unwrap(), 2, &stable_hash));
        db.insert(changed, snap("/tmp/changed.md", 20, "bb"));
        db.insert(gone, snap("/tmp/gone.md", 5, "cc"));

        let p = plan(disk, db);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].file, new);
        assert_eq!(p.mtime_touches.len(), 1);
        assert_eq!(p.mtime_touches[0].snap.content_hash, "bb");
        assert_eq!(p.deletes.len(), 1);
        assert_eq!(p.deletes[0].file_ref, "/tmp/gone.md");
    }

    #[test]
    fn plan_deletes_are_sorted_by_file_ref_for_determinism() {
        let disk: Vec<(FileRef, Mtime)> = Vec::new();
        let mut db = HashMap::new();
        db.insert(fref("/tmp/z.md"), snap("/tmp/z.md", 1, "a"));
        db.insert(fref("/tmp/a.md"), snap("/tmp/a.md", 1, "b"));
        db.insert(fref("/tmp/m.md"), snap("/tmp/m.md", 1, "c"));
        let p = plan(disk, db);
        let refs: Vec<_> = p.deletes.iter().map(|r| r.file_ref.clone()).collect();
        assert_eq!(refs, vec!["/tmp/a.md", "/tmp/m.md", "/tmp/z.md"]);
    }
}
