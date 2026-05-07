use std::collections::BTreeMap;
use std::time::Instant;

use rusqlite::Connection;

use crate::domains::common::Result;
use crate::domains::embeddings::EmbedBatch;
use crate::domains::search::{search, Fusion};

use super::bucket::build_docs;
use super::types::{GroundResponse, Stats};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundOpts {
    pub top_files: usize,
    pub chunks_per_file: usize,
    pub fusion: Fusion,
    pub limit: usize,
}

impl Default for GroundOpts {
    fn default() -> Self {
        Self {
            top_files: 10,
            chunks_per_file: 3,
            fusion: Fusion::default(),
            limit: 50,
        }
    }
}

pub fn ground(
    query: &str,
    conn: &Connection,
    embedder: &mut dyn EmbedBatch,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();
    let hits = search(conn, embedder, query, opts.fusion, opts.limit)?;
    let stats = Stats {
        fts_hits: hits.fts_hits,
        vec_hits: hits.vec_hits,
        fused: hits.fused.len(),
    };
    let docs = build_docs(conn, &hits.fused, opts.top_files, opts.chunks_per_file)?;
    Ok(GroundResponse {
        query: query.to_string(),
        took_ms: started.elapsed().as_millis() as u64,
        stats,
        docs,
        code: BTreeMap::new(),
        warnings: vec![],
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::pool::open_db;
    use crate::adapters::sqlite::queries::{
        insert_chunk, insert_vec, upsert_file, NewChunk, NewFile,
    };
    use crate::adapters::sqlite::schema::apply_schema;
    use crate::domains::embeddings::EMBEDDING_DIM;

    struct FakeEmbedder {
        vector: [f32; EMBEDDING_DIM],
    }

    impl EmbedBatch for FakeEmbedder {
        fn embed_batch(&mut self, _texts: &[String]) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(vec![self.vector])
        }
    }

    fn unit_basis(idx: usize) -> [f32; EMBEDDING_DIM] {
        let mut v = [0.0f32; EMBEDDING_DIM];
        v[idx] = 1.0;
        v
    }

    fn fresh_conn() -> Connection {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    struct Seed<'a> {
        file_ref: &'a str,
        mtime_ms: i64,
        summary: Option<&'a str>,
        keywords_json: &'a str,
        ord: i64,
        heading_path_json: &'a str,
        line_start: i64,
        line_end: i64,
        text: &'a str,
        embedding: [f32; EMBEDDING_DIM],
    }

    fn seed_chunk(conn: &Connection, s: &Seed<'_>) -> i64 {
        let file_id = upsert_file(
            conn,
            &NewFile {
                file_ref: s.file_ref,
                corpus: "docs",
                mtime_ms: s.mtime_ms,
                content_hash: s.file_ref,
                summary: s.summary,
                keywords_json: s.keywords_json,
                indexed_at_ms: 0,
            },
        )
        .expect("upsert file");
        let chunk_id = insert_chunk(
            conn,
            &NewChunk {
                file_id,
                ord: s.ord,
                heading_path_json: s.heading_path_json,
                line_start: s.line_start,
                line_end: s.line_end,
                text: s.text,
            },
        )
        .expect("insert chunk");
        insert_vec(conn, chunk_id, &s.embedding).expect("insert vec");
        chunk_id
    }

    #[test]
    fn ground_groups_chunks_under_file_and_uses_best_score() {
        let conn = fresh_conn();
        let top = seed_chunk(
            &conn,
            &Seed {
                file_ref: "/tmp/a.md",
                mtime_ms: 1_700_000_000_000,
                summary: Some("Spice doc"),
                keywords_json: "[\"spice\",\"melange\"]",
                ord: 0,
                heading_path_json: "[\"Arrakis\"]",
                line_start: 10,
                line_end: 20,
                text: "the spice melange flows on Arrakis",
                embedding: unit_basis(0),
            },
        );
        seed_chunk(
            &conn,
            &Seed {
                file_ref: "/tmp/a.md",
                mtime_ms: 1_700_000_000_000,
                summary: Some("Spice doc"),
                keywords_json: "[\"spice\",\"melange\"]",
                ord: 1,
                heading_path_json: "[\"Bene Gesserit\"]",
                line_start: 30,
                line_end: 40,
                text: "wisdom of the Bene Gesserit",
                embedding: unit_basis(7),
            },
        );
        seed_chunk(
            &conn,
            &Seed {
                file_ref: "/tmp/b.md",
                mtime_ms: 1_700_000_001_000,
                summary: Some("Fremen doc"),
                keywords_json: "[\"fremen\"]",
                ord: 0,
                heading_path_json: "[]",
                line_start: 5,
                line_end: 12,
                text: "fremen ride sandworms across dunes",
                embedding: unit_basis(3),
            },
        );

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let response = ground(
            "spice",
            &conn,
            &mut embedder,
            GroundOpts {
                top_files: 5,
                chunks_per_file: 3,
                fusion: Fusion::Rrf { k: 60 },
                limit: 10,
            },
        )
        .expect("ground");

        assert_eq!(response.query, "spice");
        assert_eq!(response.stats.fts_hits, 1);
        assert!(response.stats.vec_hits >= 1);
        assert!(response.stats.fused >= 1);

        let a = response.docs.get("/tmp/a.md").expect("file a present");
        assert_eq!(a.corpus, "docs");
        assert_eq!(a.summary.as_deref(), Some("Spice doc"));
        assert_eq!(a.keywords, vec!["spice".to_string(), "melange".into()]);
        assert_eq!(a.mtime, "2023-11-14T22:13:20Z");
        assert!(a.fts_rank.is_some(), "best chunk hit FTS");
        assert!(a.vec_rank.is_some(), "best chunk hit vec");
        let best = a.chunks.first().expect("chunk present");
        assert_eq!(best.chunk_id, top);
        assert_eq!(best.line_range, [10, 20]);
        assert_eq!(best.heading_path, vec!["Arrakis".to_string()]);
        assert!((a.score - best.score).abs() < 1e-12);
        assert!(best.snippet.contains("spice"));
    }

    #[test]
    fn ground_caps_top_files_and_chunks_per_file() {
        let conn = fresh_conn();
        for (i, path) in ["/tmp/a.md", "/tmp/b.md", "/tmp/c.md"].iter().enumerate() {
            for ord in 0..3 {
                seed_chunk(
                    &conn,
                    &Seed {
                        file_ref: path,
                        mtime_ms: 0,
                        summary: None,
                        keywords_json: "[]",
                        ord,
                        heading_path_json: "[]",
                        line_start: 1,
                        line_end: 2,
                        text: "spice spice spice",
                        embedding: unit_basis(i * 10 + ord as usize),
                    },
                );
            }
        }

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let response = ground(
            "spice",
            &conn,
            &mut embedder,
            GroundOpts {
                top_files: 2,
                chunks_per_file: 1,
                fusion: Fusion::Rrf { k: 60 },
                limit: 50,
            },
        )
        .expect("ground");

        assert_eq!(response.docs.len(), 2, "must cap at top_files=2");
        for doc in response.docs.values() {
            assert_eq!(doc.chunks.len(), 1, "must cap at chunks_per_file=1");
        }
    }
}
