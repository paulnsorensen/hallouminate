use std::collections::{BTreeMap, HashMap};

use crate::adapters::lance::SearchHit;
use crate::domain::common::Result;
use crate::domain::corpus::make_snippet;

use super::types::{ChunkProvenance, DocChunk, DocFile};

/// Bucket `hits` by `file_ref`, sort by max-score descending (file_ref tiebreak),
/// truncate to `top_files`, then take the top `chunks_per_file` chunks per
/// bucket by score. Pure CPU — LanceDB hits already carry every field needed
/// to render a `DocFile`, including `mtime_ms` which is formatted as RFC3339
/// at bucket-emit time.
pub(super) fn build_docs(
    hits: &[SearchHit],
    top_files: usize,
    chunks_per_file: usize,
) -> Result<BTreeMap<String, DocFile>> {
    let mut buckets: HashMap<String, FileBucket> = HashMap::new();
    for hit in hits {
        buckets
            .entry(hit.file_ref.clone())
            .or_insert_with(|| FileBucket::new(hit))
            .push(hit.clone());
    }
    let mut files: Vec<FileBucket> = buckets.into_values().collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_ref.cmp(&b.file_ref))
    });
    files.truncate(top_files);
    let mut out = BTreeMap::new();
    for f in files {
        let (key, doc) = f.into_doc(chunks_per_file);
        out.insert(key, doc);
    }
    Ok(out)
}

struct FileBucket {
    file_ref: String,
    summary: Option<String>,
    keywords: Vec<String>,
    score: f64,
    mtime_ms: i64,
    chunks: Vec<SearchHit>,
}

impl FileBucket {
    fn new(hit: &SearchHit) -> Self {
        Self {
            file_ref: hit.file_ref.clone(),
            summary: option_from(&hit.summary),
            keywords: hit.keywords.clone(),
            score: f64::MIN,
            mtime_ms: hit.mtime_ms,
            chunks: Vec::new(),
        }
    }

    fn push(&mut self, hit: SearchHit) {
        let s = hit.score as f64;
        if s > self.score {
            self.score = s;
        }
        self.chunks.push(hit);
    }

    fn into_doc(mut self, chunks_per_file: usize) -> (String, DocFile) {
        self.chunks.sort_by(|a, b| {
            (b.score)
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk_id.cmp(&b.chunk_id))
        });
        self.chunks.truncate(chunks_per_file);
        let chunks = self
            .chunks
            .iter()
            .map(|h| DocChunk {
                chunk_id: h.chunk_id.clone(),
                heading_path: h.heading_path.clone(),
                line_range: [h.line_start as u32, h.line_end as u32],
                score: h.score as f64,
                snippet: make_snippet(&h.text),
                // Stamped by the orchestrator from its corpus arg (same as
                // DocFile.corpus); the LanceDB row implies corpus by query
                // scope and doesn't carry it per-row.
                provenance: ChunkProvenance::default(),
            })
            .collect();
        // mtime is sourced from SearchHit.mtime_ms (decoded from the
        // LanceDB `mtime_ms` column) and formatted RFC3339 in UTC with
        // second precision so the response shape matches `2026-04-30T10:11:23Z`.
        // An out-of-range timestamp collapses to an empty string — the
        // documented contract — rather than panicking on bad row data.
        // corpus is stamped in by the orchestrator from its `corpus: &str`
        // arg, since the LanceDB row's corpus is implied by the query
        // scope (`corpus = '...'`) and isn't returned per-row.
        let mtime = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(self.mtime_ms)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default();
        (
            self.file_ref,
            DocFile {
                summary: self.summary,
                keywords: self.keywords,
                score: self.score,
                mtime,
                corpus: String::new(),
                chunks,
            },
        )
    }
}

fn option_from(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-01-01T00:00:00Z — fixed reference timestamp so the RFC3339
    // assertion below stays readable.
    const FIXTURE_MTIME_MS: i64 = 1_704_067_200_000;

    fn hit(file_ref: &str, ord: usize, score: f32) -> SearchHit {
        SearchHit {
            chunk_id: format!("{file_ref}#{ord}"),
            file_ref: file_ref.into(),
            heading_path: vec!["section".into()],
            line_start: ord + 1,
            line_end: ord + 5,
            text: format!("body of {file_ref}#{ord}"),
            summary: format!("summary of {file_ref}"),
            keywords: vec!["docs".into(), "test".into()],
            score,
            mtime_ms: FIXTURE_MTIME_MS,
        }
    }

    #[test]
    fn bucket_groups_hits_by_file_and_picks_max_score() {
        let hits = vec![
            hit("/a.md", 0, 0.9),
            hit("/a.md", 1, 0.5),
            hit("/b.md", 0, 0.7),
        ];
        let docs = build_docs(&hits, 10, 10).expect("build");
        assert_eq!(docs.len(), 2);
        let a = docs.get("/a.md").expect("a present");
        assert!((a.score - 0.9_f64).abs() < 1e-6, "{} != 0.9", a.score);
        assert_eq!(a.chunks.len(), 2);
        // chunks sorted by score desc
        assert!(a.chunks[0].score >= a.chunks[1].score);
        assert_eq!(a.summary.as_deref(), Some("summary of /a.md"));
        assert_eq!(a.keywords, vec!["docs".to_string(), "test".into()]);
    }

    #[test]
    fn truncates_to_top_files_by_score_descending_tiebreak_on_path() {
        let hits = vec![
            hit("/c.md", 0, 0.5),
            hit("/a.md", 0, 0.5), // tied, lex smaller wins tie
            hit("/b.md", 0, 0.9),
        ];
        let docs = build_docs(&hits, 2, 5).expect("build");
        assert_eq!(docs.len(), 2);
        assert!(docs.contains_key("/b.md"));
        assert!(
            docs.contains_key("/a.md"),
            "lex-smaller path wins tied score; got {:?}",
            docs.keys().collect::<Vec<_>>()
        );
        assert!(!docs.contains_key("/c.md"));
    }

    #[test]
    fn truncates_chunks_per_file_keeping_highest_scores() {
        let hits = vec![
            hit("/x.md", 0, 0.9),
            hit("/x.md", 1, 0.5),
            hit("/x.md", 2, 0.7),
        ];
        let docs = build_docs(&hits, 5, 2).expect("build");
        let x = docs.get("/x.md").expect("x present");
        assert_eq!(x.chunks.len(), 2);
        assert!(
            (x.chunks[0].score - 0.9).abs() < 1e-6,
            "got {}",
            x.chunks[0].score
        );
        assert!(
            (x.chunks[1].score - 0.7).abs() < 1e-6,
            "got {}",
            x.chunks[1].score
        );
    }

    #[test]
    fn empty_hits_yield_empty_docs() {
        let docs = build_docs(&[], 10, 10).expect("build");
        assert!(docs.is_empty());
    }

    #[test]
    fn top_files_zero_yields_empty_docs_even_when_hits_present() {
        let hits = vec![hit("/a.md", 0, 0.9), hit("/b.md", 0, 0.5)];
        let docs = build_docs(&hits, 0, 5).expect("build");
        assert!(
            docs.is_empty(),
            "top_files=0 must drop every bucket; got {:?}",
            docs.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunks_per_file_zero_keeps_files_but_drops_all_chunks() {
        let hits = vec![hit("/a.md", 0, 0.9), hit("/a.md", 1, 0.5)];
        let docs = build_docs(&hits, 5, 0).expect("build");
        assert_eq!(docs.len(), 1, "file bucket survives top-files cut");
        let a = docs.get("/a.md").expect("a present");
        assert!(a.chunks.is_empty(), "chunks_per_file=0 drops all chunks");
        // Bucket-level summary/keywords/score still reflect the underlying hits.
        assert!((a.score - 0.9).abs() < 1e-6, "bucket score preserved");
        assert_eq!(a.summary.as_deref(), Some("summary of /a.md"));
    }

    #[test]
    fn empty_summary_string_renders_as_none() {
        // SearchHit.summary is a String (LanceDB column is non-null); the
        // domain contract is that an empty string surfaces as None on
        // DocFile.summary so JSON consumers see `null`, not `""`.
        let mut h = hit("/a.md", 0, 0.5);
        h.summary = String::new();
        let docs = build_docs(&[h], 5, 5).expect("build");
        let a = docs.get("/a.md").expect("a present");
        assert_eq!(
            a.summary, None,
            "empty SearchHit.summary must collapse to None"
        );
    }

    #[test]
    fn mtime_ms_is_formatted_as_rfc3339_utc_with_z_suffix() {
        // Regression for PR #7 Copilot review: DocFile.mtime must reflect
        // the file's stored mtime_ms, not an empty string. Use the fixture
        // hit's mtime_ms (2024-01-01T00:00:00Z) so the round-trip is exact.
        let docs = build_docs(&[hit("/a.md", 0, 0.5)], 5, 5).expect("build");
        let a = docs.get("/a.md").expect("a present");
        assert_eq!(
            a.mtime, "2024-01-01T00:00:00Z",
            "DocFile.mtime must be RFC3339(seconds, Z) from SearchHit.mtime_ms"
        );
    }

    #[test]
    fn out_of_range_mtime_ms_collapses_to_empty_string() {
        // Defensive: an unrepresentable timestamp must not panic. Pick a
        // value beyond chrono's i64-ms range so from_timestamp_millis() is
        // None and the formatter returns Default (empty string).
        let mut h = hit("/a.md", 0, 0.5);
        h.mtime_ms = i64::MAX;
        let docs = build_docs(&[h], 5, 5).expect("build");
        let a = docs.get("/a.md").expect("a present");
        assert!(
            a.mtime.is_empty(),
            "out-of-range mtime_ms must collapse to empty, got {:?}",
            a.mtime
        );
    }

    #[test]
    fn chunks_tiebreak_on_chunk_id_ascending_when_scores_equal() {
        // Two chunks with identical score should sort by chunk_id asc so the
        // output is deterministic across runs.
        let hits = vec![
            hit("/x.md", 2, 0.5), // chunk_id "/x.md#2"
            hit("/x.md", 0, 0.5), // chunk_id "/x.md#0"
            hit("/x.md", 1, 0.5), // chunk_id "/x.md#1"
        ];
        let docs = build_docs(&hits, 5, 5).expect("build");
        let x = docs.get("/x.md").expect("x present");
        let ids: Vec<&str> = x.chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["/x.md#0", "/x.md#1", "/x.md#2"],
            "tied scores must sort by chunk_id ascending"
        );
    }
}
