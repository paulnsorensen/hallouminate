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
    z_score: Option<f64>,
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
            z_score: None,
            mtime_ms: hit.mtime_ms,
            chunks: Vec::new(),
        }
    }
    fn push(&mut self, hit: SearchHit) {
        let s = hit.score as f64;
        if s > self.score {
            self.score = s;
            self.z_score = hit.z_score;
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
                z_score: h.z_score,
                snippet: make_snippet(&h.text),
                // `corpus` is stamped by the orchestrator from its corpus arg
                // (the LanceDB row implies corpus by query scope and doesn't
                // carry it per-row); `claim_marks` is per-row, so it flows from
                // the decoded hit here.
                provenance: ChunkProvenance {
                    claim_marks: h.claim_marks.clone(),
                    ..ChunkProvenance::default()
                },
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
                z_score: self.z_score,
                mtime,
                corpus: String::new(),
                path: None,
                stale: false,
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

/// Per-query z-score over candidate scores. Returns one `Option<f64>` per hit,
/// index-aligned to `hits`. `None` for the small-n and sigma~0 degenerate
/// cases — a per-query RELATIVE score, NOT a calibrated probability.
///
/// Population std (divide by n), not sample (n-1): we have the entire
/// candidate population for this query, not a sample of it.
pub(super) fn normalize_scores(hits: &[SearchHit]) -> Vec<Option<f64>> {
    const MIN_N: usize = 5; // n < ~5 -> z is noisy
    const SIGMA_EPS: f64 = 1e-9; // all-equal scores -> sigma 0
    let n = hits.len();
    if n < MIN_N {
        return vec![None; n];
    }
    let scores: Vec<f64> = hits.iter().map(|h| h.score as f64).collect();
    let mu = scores.iter().sum::<f64>() / n as f64;
    let var = scores.iter().map(|s| (s - mu).powi(2)).sum::<f64>() / n as f64;
    let sigma = var.sqrt();
    if sigma <= SIGMA_EPS {
        return vec![None; n]; // degenerate: no spread to normalize against
    }
    scores.iter().map(|s| Some((s - mu) / sigma)).collect()
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
            claim_marks: vec![],
            z_score: None,
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
    fn claim_marks_flow_from_hit_into_chunk_provenance() {
        use crate::domain::corpus::{ClaimMark, ClaimStatus};
        let mut h = hit("/a.md", 0, 0.9);
        h.claim_marks = vec![ClaimMark {
            status: ClaimStatus::Superseded,
            line: 12,
            reference: Some("old.md".into()),
            note: None,
        }];
        let docs = build_docs(&[h], 5, 5).expect("build");
        let chunk = &docs.get("/a.md").expect("a present").chunks[0];
        assert_eq!(
            chunk.provenance.claim_marks,
            vec![ClaimMark {
                status: ClaimStatus::Superseded,
                line: 12,
                reference: Some("old.md".into()),
                note: None,
            }],
            "per-row claim marks must surface on DocChunk.provenance, not be dropped"
        );
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
    // --- #141: normalize_scores and z_score propagation ---

    fn hit_with_score(score: f32) -> SearchHit {
        SearchHit {
            chunk_id: format!("c#{score}"),
            file_ref: "/f.md".into(),
            heading_path: vec![],
            line_start: 1,
            line_end: 2,
            text: String::new(),
            summary: String::new(),
            keywords: vec![],
            score,
            mtime_ms: FIXTURE_MTIME_MS,
            claim_marks: vec![],
            z_score: None,
        }
    }

    #[test]
    fn normalize_scores_normal_spread_produces_valid_z_scores() {
        // WHY: z is a real per-query normalization, not a passthrough.
        // Mean of z must be ~0, population std of z must be ~1, max-score hit
        // has the largest z, every value must be Some.
        let hits: Vec<SearchHit> = [0.9_f32, 0.7, 0.5, 0.3, 0.1]
            .into_iter()
            .map(hit_with_score)
            .collect();
        let zs = normalize_scores(&hits);
        assert_eq!(zs.len(), 5);
        let zvals: Vec<f64> = zs
            .iter()
            .map(|z| z.expect("all Some for normal spread"))
            .collect();
        // max-score hit (score=0.9) must have the largest z
        assert!(
            zvals[0] > zvals[1] && zvals[1] > zvals[4],
            "z must preserve score ordering: {zvals:?}"
        );
        let mean_z: f64 = zvals.iter().sum::<f64>() / zvals.len() as f64;
        assert!(mean_z.abs() < 1e-9, "mean(z) must be ~0, got {mean_z}");
        let var_z: f64 =
            zvals.iter().map(|z| (z - mean_z).powi(2)).sum::<f64>() / zvals.len() as f64;
        let std_z = var_z.sqrt();
        assert!(
            (std_z - 1.0).abs() < 1e-9,
            "popstd(z) must be ~1, got {std_z}"
        );
    }

    #[test]
    fn normalize_scores_sigma_zero_emits_none() {
        // WHY: degenerate pool has no spread; emitting a number would be a lie
        // (decision 3). Encoding the caveat that spreadless scores must not normalize.
        let hits: Vec<SearchHit> = std::iter::repeat_n(hit_with_score(0.5), 5).collect();
        let zs = normalize_scores(&hits);
        assert!(
            zs.iter().all(|z| z.is_none()),
            "all-equal scores (sigma~0) must emit None, not a number: {zs:?}"
        );
    }

    #[test]
    fn normalize_scores_small_n_emits_none() {
        // WHY: below MIN_N the estimate is noise; None not 0 (decision 3).
        let hits: Vec<SearchHit> = [0.9_f32, 0.5, 0.1]
            .into_iter()
            .map(hit_with_score)
            .collect();
        let zs = normalize_scores(&hits);
        assert_eq!(zs.len(), 3);
        assert!(
            zs.iter().all(|z| z.is_none()),
            "n=3 < MIN_N must emit None for every hit: {zs:?}"
        );
    }

    #[test]
    fn rrf_mode_docs_have_no_z_score() {
        // WHY: encodes the caveat that RRF scores must not be normalized
        // (decision 4). The stamp loop in `ground`/`ground_union` is lexically
        // inside the `if let Some(rerank)` cross-encoder block, so without a
        // cross-encoder every `SearchHit` arrives at `build_docs` with
        // `z_score: None` — the decode default set in `lance.rs:481`. This
        // test verifies that `build_docs` faithfully propagates that None to
        // `DocFile.z_score` and `DocChunk.z_score` on the wire. `build_docs`
        // is the structural enforcement point: it reads `hit.z_score` directly,
        // so if the orchestrator's gate held (no stamp → None), the wire types
        // are guaranteed None without any additional guard here.
        let hits = vec![
            hit("/a.md", 0, 0.9),
            hit("/a.md", 1, 0.5),
            hit("/b.md", 0, 0.7),
            hit("/c.md", 0, 0.6),
            hit("/d.md", 0, 0.3),
        ];
        // No z stamping — simulates the RRF/OFF path (crossencoder: None, so the
        // `if let Some(rerank)` gate in the orchestrator never fires).
        let docs = build_docs(&hits, 10, 10).expect("build");
        for (path, doc) in &docs {
            assert!(
                doc.z_score.is_none(),
                "DocFile {path} must have z_score=None on RRF/OFF path"
            );
            for chunk in &doc.chunks {
                assert!(
                    chunk.z_score.is_none(),
                    "DocChunk {path}/{} must have z_score=None on RRF/OFF path",
                    chunk.chunk_id
                );
            }
        }
    }

    #[test]
    fn pre_truncation_scope_z_uses_full_pool_mu_sigma() {
        // WHY: z must not depend on top_files (decision 2). The surviving docs'
        // z values must match what you'd compute from the full-pool mu/sigma,
        // not a re-normalization over only the top-N.
        let scores: Vec<f32> = [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.05].to_vec();
        let mut hits: Vec<SearchHit> = scores
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let mut h = hit(&format!("/f{i}.md"), 0, s);
                h.z_score = None;
                h
            })
            .collect();
        // Stamp z from the full pool (10 hits) — mirrors what the orchestrator does.
        let zs = normalize_scores(&hits);
        for (h, z) in hits.iter_mut().zip(&zs) {
            h.z_score = *z;
        }
        // Compute expected z for the top-2 hits from the FULL 10-hit pool.
        let all_scores: Vec<f64> = scores.iter().map(|&s| s as f64).collect();
        let mu = all_scores.iter().sum::<f64>() / 10.0;
        let var = all_scores.iter().map(|s| (s - mu).powi(2)).sum::<f64>() / 10.0;
        let sigma = var.sqrt();
        let expected_top_z = (0.9_f32 as f64 - mu) / sigma;
        let expected_second_z = (0.8_f32 as f64 - mu) / sigma;
        // Now build docs with top_files=2 — only top-2 survive.
        let docs = build_docs(&hits, 2, 5).expect("build");
        assert_eq!(docs.len(), 2);
        let top = docs.get("/f0.md").expect("/f0.md present");
        assert!(
            (top.z_score.expect("top file z must be Some") - expected_top_z).abs() < 1e-9,
            "top file z must match full-pool normalization"
        );
        let second = docs.get("/f1.md").expect("/f1.md present");
        assert!(
            (second.z_score.expect("second file z must be Some") - expected_second_z).abs() < 1e-9,
            "second file z must match full-pool normalization"
        );
    }

    #[test]
    fn z_score_absent_in_legacy_payload_deserializes_as_none() {
        // WHY: #[serde(default)] keeps strict-schema clients from breaking
        // (decision 1). Mirrors the provenance back-compat precedent (#106).
        use serde_json::json;
        let legacy_file = json!({
            "summary": null,
            "keywords": [],
            "score": 0.5,
            "mtime": "2026-01-01T00:00:00Z",
            "corpus": "docs",
            "chunks": []
        });
        let doc: super::super::types::DocFile =
            serde_json::from_value(legacy_file).expect("must deserialize");
        assert!(
            doc.z_score.is_none(),
            "absent z_score field must default to None, not error"
        );
        let legacy_chunk = json!({
            "chunk_id": "abc",
            "heading_path": [],
            "line_range": [1, 2],
            "score": 0.5,
            "snippet": "text"
        });
        let chunk: super::super::types::DocChunk =
            serde_json::from_value(legacy_chunk).expect("must deserialize");
        assert!(
            chunk.z_score.is_none(),
            "absent z_score field on DocChunk must default to None"
        );
    }

    #[test]
    fn z_score_none_serializes_as_json_null() {
        // WHY: confirms the Option->null wire contract (house convention).
        use serde_json::json;
        let file = super::super::types::DocFile {
            summary: None,
            keywords: vec![],
            score: 0.5,
            z_score: None,
            mtime: "2026-01-01T00:00:00Z".into(),
            corpus: "docs".into(),
            path: None,
            stale: false,
            chunks: vec![],
        };
        let v = serde_json::to_value(&file).expect("serialize");
        assert_eq!(
            v["z_score"],
            json!(null),
            "z_score: None must serialize as null"
        );
    }

    #[test]
    fn normalize_scores_boundary_n4_none_n5_some() {
        // WHY: MIN_N is 5; n=4 is BELOW the threshold (must yield None), n=5 is
        // AT the threshold (must yield Some). The boundary is a code path fork
        // at `if n < MIN_N` — both sides must be exercised, not just n=3.
        let hits4: Vec<SearchHit> = [0.9_f32, 0.7, 0.5, 0.3]
            .into_iter()
            .map(hit_with_score)
            .collect();
        let zs4 = normalize_scores(&hits4);
        assert_eq!(zs4.len(), 4);
        assert!(
            zs4.iter().all(|z| z.is_none()),
            "n=4 < MIN_N must emit None for every hit: {zs4:?}"
        );
        // n=5: exactly at the threshold — must produce Some values.
        let hits5: Vec<SearchHit> = [0.9_f32, 0.7, 0.5, 0.3, 0.1]
            .into_iter()
            .map(hit_with_score)
            .collect();
        let zs5 = normalize_scores(&hits5);
        assert_eq!(zs5.len(), 5);
        assert!(
            zs5.iter().all(|z| z.is_some()),
            "n=5 == MIN_N must emit Some for every hit (boundary is inclusive): {zs5:?}"
        );
    }

    #[test]
    fn file_bucket_z_score_tracks_max_score_chunk() {
        // WHY: FileBucket.push adopts z alongside score when a higher-score chunk
        // arrives (decision 1b). If the first chunk is low-score and the second is
        // high-score, the file's z_score must reflect the HIGH-score chunk's z,
        // not the first chunk's. Regression: wrong ordering in push() would
        // silently propagate the wrong z to DocFile.
        let mut low = hit("/a.md", 0, 0.3);
        low.z_score = Some(-1.0); // z for the lower-score chunk
        let mut high = hit("/a.md", 1, 0.9);
        high.z_score = Some(1.5); // z for the higher-score chunk
                                  // Push low first, then high — FileBucket.push should adopt high's z.
        let docs = build_docs(&[low, high], 5, 5).expect("build");
        let a = docs.get("/a.md").expect("a present");
        assert!(
            (a.z_score.expect("file z must be Some") - 1.5).abs() < 1e-9,
            "DocFile z_score must track the max-score chunk's z, got {:?}",
            a.z_score
        );
    }

    #[test]
    fn chunk_z_score_populated_from_hit_z_score() {
        // WHY: DocChunk.z_score comes from hit.z_score in into_doc. If the wiring
        // is broken the chunk gets None even when the orchestrator stamped z.
        // This test exercises the chunk propagation path directly.
        let mut h0 = hit("/a.md", 0, 0.9);
        h0.z_score = Some(1.2);
        let mut h1 = hit("/a.md", 1, 0.5);
        h1.z_score = Some(-0.3);
        let docs = build_docs(&[h0, h1], 5, 5).expect("build");
        let a = docs.get("/a.md").expect("a present");
        // Chunks are sorted by score desc: h0 (0.9) first, h1 (0.5) second.
        assert_eq!(a.chunks.len(), 2);
        assert!(
            (a.chunks[0].z_score.expect("chunk 0 z must be Some") - 1.2).abs() < 1e-9,
            "top chunk z_score must be 1.2, got {:?}",
            a.chunks[0].z_score
        );
        assert!(
            (a.chunks[1].z_score.expect("chunk 1 z must be Some") - (-0.3)).abs() < 1e-9,
            "second chunk z_score must be -0.3, got {:?}",
            a.chunks[1].z_score
        );
    }
}
