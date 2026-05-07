use rusqlite::Connection;

use crate::domains::common::{HallouminateError, Result};
use crate::domains::embeddings::EmbedBatch;

use super::convex::convex_fuse;
use super::fts::fts_search;
use super::rrf::rrf_fuse;
use super::vector::vec_search;

pub use super::rrf::FusedHit;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    Rrf { k: u32 },
    Convex { alpha: f32 },
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Rrf { k: 60 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchHits {
    pub fts_hits: usize,
    pub vec_hits: usize,
    pub fused: Vec<FusedHit>,
}

pub fn search(
    conn: &Connection,
    embedder: &mut dyn EmbedBatch,
    query: &str,
    fusion: Fusion,
    limit: usize,
) -> Result<SearchHits> {
    let mut embeds = embedder.embed_batch(&[query.to_string()])?;
    let query_vec = embeds
        .pop()
        .ok_or_else(|| HallouminateError::Embed("empty embedding for query".into()))?;
    let fts = fts_search(conn, query, limit)?;
    let vec = vec_search(conn, &query_vec, limit)?;
    let fts_hits = fts.len();
    let vec_hits = vec.len();
    let mut fused = match fusion {
        Fusion::Rrf { k } => rrf_fuse(&fts, &vec, k),
        Fusion::Convex { alpha } => convex_fuse(&fts, &vec, alpha),
    };
    fused.truncate(limit);
    Ok(SearchHits {
        fts_hits,
        vec_hits,
        fused,
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
    use crate::domains::common::ChunkId;
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

    fn seed(conn: &Connection, file_ref: &str, text: &str, embedding: [f32; EMBEDDING_DIM]) -> i64 {
        let file_id = upsert_file(
            conn,
            &NewFile {
                file_ref,
                corpus: "docs",
                mtime_ms: 0,
                content_hash: file_ref,
                summary: None,
                keywords_json: "[]",
                indexed_at_ms: 0,
            },
        )
        .expect("upsert file");
        let chunk_id = insert_chunk(
            conn,
            &NewChunk {
                file_id,
                ord: 0,
                heading_path_json: "[]",
                line_start: 1,
                line_end: 4,
                text,
            },
        )
        .expect("insert chunk");
        insert_vec(conn, chunk_id, &embedding).expect("insert vec");
        chunk_id
    }

    #[test]
    fn search_rrf_top_chunk_appears_in_both_modalities() {
        let conn = fresh_conn();
        let strong = seed(&conn, "/tmp/a.md", "spice melange flows", unit_basis(0));
        seed(&conn, "/tmp/b.md", "barely related text", unit_basis(7));

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let result =
            search(&conn, &mut embedder, "spice", Fusion::Rrf { k: 60 }, 10).expect("search");

        assert_eq!(result.fused[0].chunk_id, ChunkId(strong));
        assert!(
            result.fused[0].fts_rank.is_some() && result.fused[0].vec_rank.is_some(),
            "winner must appear in both modalities: {:?}",
            result.fused
        );
        assert_eq!(result.fts_hits, 1);
        assert_eq!(result.vec_hits, 2);
    }

    #[test]
    fn search_truncates_to_limit() {
        let conn = fresh_conn();
        for i in 0..3 {
            let path = format!("/tmp/{i}.md");
            seed(&conn, &path, "spice flows", unit_basis(i));
        }

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let result =
            search(&conn, &mut embedder, "spice", Fusion::Rrf { k: 60 }, 2).expect("search");
        assert_eq!(result.fused.len(), 2);
    }

    #[test]
    fn search_convex_alpha_one_picks_fts_winner() {
        let conn = fresh_conn();
        let fts_winner = seed(
            &conn,
            "/tmp/a.md",
            "spice spice spice spice melange",
            unit_basis(99),
        );
        seed(&conn, "/tmp/b.md", "unrelated text", unit_basis(0));

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let result = search(
            &conn,
            &mut embedder,
            "spice",
            Fusion::Convex { alpha: 1.0 },
            5,
        )
        .expect("search");
        assert_eq!(result.fused[0].chunk_id, ChunkId(fts_winner));
    }

    #[test]
    fn search_convex_alpha_zero_picks_vec_winner() {
        let conn = fresh_conn();
        seed(
            &conn,
            "/tmp/a.md",
            "spice spice spice spice melange",
            unit_basis(99),
        );
        let vec_winner = seed(&conn, "/tmp/b.md", "unrelated text", unit_basis(0));

        let mut embedder = FakeEmbedder {
            vector: unit_basis(0),
        };
        let result = search(
            &conn,
            &mut embedder,
            "spice",
            Fusion::Convex { alpha: 0.0 },
            5,
        )
        .expect("search");
        assert_eq!(result.fused[0].chunk_id, ChunkId(vec_winner));
    }

    #[test]
    fn search_default_fusion_is_rrf_with_k_60() {
        match Fusion::default() {
            Fusion::Rrf { k } => assert_eq!(k, 60),
            other => panic!("expected RRF default, got {other:?}"),
        }
    }
}
