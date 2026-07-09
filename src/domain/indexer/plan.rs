use std::collections::HashMap;

use crate::domain::common::{FileRef, Mtime};

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
/// three actions [`apply`](crate::domain::indexer::apply) performs: write new/changed
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
            Some(snap) if snap.mtime_ms == mtime.0 => continue,
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
    fn plan_skips_files_with_unchanged_mtime() {
        let file = fref("/tmp/stable.md");
        let disk = vec![(file.clone(), Mtime(100))];
        let mut db = HashMap::new();
        db.insert(file, snap("/tmp/stable.md", 100, "deadbeef"));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.mtime_touches.is_empty());
        assert!(p.deletes.is_empty());
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

    #[test]
    fn plan_handles_full_matrix_simultaneously() {
        let new = fref("/tmp/new.md");
        let stable = fref("/tmp/stable.md");
        let changed = fref("/tmp/changed.md");
        let gone = fref("/tmp/gone.md");
        let disk = vec![
            (new.clone(), Mtime(1)),
            (stable.clone(), Mtime(2)),
            (changed.clone(), Mtime(30)),
        ];
        let mut db = HashMap::new();
        db.insert(stable, snap("/tmp/stable.md", 2, "aa"));
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
