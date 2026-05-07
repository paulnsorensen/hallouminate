use std::collections::HashMap;

use crate::adapters::sqlite::FileRow;
use crate::domain::common::{FileRef, Mtime};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexPlan {
    pub upserts: Vec<Upsert>,
    pub mtime_touches: Vec<MtimeCandidate>,
    pub deletes: Vec<FileRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upsert {
    pub file: FileRef,
    pub mtime: Mtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtimeCandidate {
    pub file: FileRef,
    pub row: FileRow,
    pub new_mtime: Mtime,
}

pub fn plan(disk: Vec<(FileRef, Mtime)>, mut db: HashMap<FileRef, FileRow>) -> IndexPlan {
    let mut out = IndexPlan::default();
    for (file, mtime) in disk {
        match db.remove(&file) {
            None => out.upserts.push(Upsert { file, mtime }),
            Some(row) if row.mtime_ms == mtime.0 => continue,
            Some(row) => out.mtime_touches.push(MtimeCandidate {
                file,
                row,
                new_mtime: mtime,
            }),
        }
    }
    let mut leftover: Vec<FileRow> = db.into_values().collect();
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

    fn row(file_id: i64, file_ref: &str, mtime_ms: i64, hash: &str) -> FileRow {
        FileRow {
            file_id,
            file_ref: file_ref.to_string(),
            corpus: "docs".to_string(),
            mtime_ms,
            content_hash: hash.to_string(),
            summary: None,
            keywords_json: "[]".to_string(),
            indexed_at_ms: 1_700_000_000_000,
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
        db.insert(file, row(1, "/tmp/stable.md", 100, "deadbeef"));
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
        db.insert(file.clone(), row(7, "/tmp/changed.md", 100, "cafebabe"));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.deletes.is_empty());
        assert_eq!(p.mtime_touches.len(), 1);
        let cand = &p.mtime_touches[0];
        assert_eq!(cand.file, file);
        assert_eq!(cand.new_mtime, Mtime(200));
        assert_eq!(cand.row.file_id, 7);
        assert_eq!(cand.row.mtime_ms, 100);
        assert_eq!(cand.row.content_hash, "cafebabe");
    }

    #[test]
    fn plan_routes_vanished_files_into_deletes() {
        let disk: Vec<(FileRef, Mtime)> = Vec::new();
        let mut db = HashMap::new();
        db.insert(fref("/tmp/gone.md"), row(3, "/tmp/gone.md", 50, "f00d"));
        let p = plan(disk, db);
        assert!(p.upserts.is_empty());
        assert!(p.mtime_touches.is_empty());
        assert_eq!(p.deletes.len(), 1);
        assert_eq!(p.deletes[0].file_id, 3);
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
        db.insert(stable, row(10, "/tmp/stable.md", 2, "aa"));
        db.insert(changed, row(11, "/tmp/changed.md", 20, "bb"));
        db.insert(gone, row(12, "/tmp/gone.md", 5, "cc"));

        let p = plan(disk, db);
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].file, new);
        assert_eq!(p.mtime_touches.len(), 1);
        assert_eq!(p.mtime_touches[0].row.file_id, 11);
        assert_eq!(p.deletes.len(), 1);
        assert_eq!(p.deletes[0].file_id, 12);
    }

    #[test]
    fn plan_deletes_are_sorted_by_file_ref_for_determinism() {
        let disk: Vec<(FileRef, Mtime)> = Vec::new();
        let mut db = HashMap::new();
        db.insert(fref("/tmp/z.md"), row(1, "/tmp/z.md", 1, "a"));
        db.insert(fref("/tmp/a.md"), row(2, "/tmp/a.md", 1, "b"));
        db.insert(fref("/tmp/m.md"), row(3, "/tmp/m.md", 1, "c"));
        let p = plan(disk, db);
        let refs: Vec<_> = p.deletes.iter().map(|r| r.file_ref.clone()).collect();
        assert_eq!(refs, vec!["/tmp/a.md", "/tmp/m.md", "/tmp/z.md"]);
    }
}
