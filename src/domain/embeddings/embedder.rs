use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::{params, OptionalExtension};

use crate::adapters::sqlite::DbConn;
use crate::domain::common::{HallouminateError, Result};

pub const EMBEDDING_DIM: usize = 384;
pub const DEFAULT_MODEL: &str = "bge-small-en-v1.5";
const ALT_MODEL: &str = "all-minilm-l6-v2";
const META_KEY_MODEL: &str = "embeddings.model";

pub trait EmbedBatch {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<[f32; EMBEDDING_DIM]>>;
}

pub struct Embedder {
    inner: TextEmbedding,
    model_name: String,
}

impl Embedder {
    pub fn try_new(model_name: &str, cache_dir: &Path, conn: &DbConn) -> Result<Self> {
        check_or_set_model(conn, model_name)?;
        let model = resolve_model(model_name)?;
        let opts = TextInitOptions::new(model)
            .with_cache_dir(PathBuf::from(cache_dir))
            .with_show_download_progress(false);
        let inner = TextEmbedding::try_new(opts)
            .map_err(|e| HallouminateError::Embed(format!("init {model_name}: {e}")))?;
        Ok(Self {
            inner,
            model_name: model_name.to_string(),
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

impl EmbedBatch for Embedder {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let raw = self
            .inner
            .embed(texts, None)
            .map_err(|e| HallouminateError::Embed(format!("embed: {e}")))?;
        raw.into_iter().map(finalize_vector).collect()
    }
}

fn finalize_vector(v: Vec<f32>) -> Result<[f32; EMBEDDING_DIM]> {
    let mut arr: [f32; EMBEDDING_DIM] = v.try_into().map_err(|v: Vec<f32>| {
        HallouminateError::Embed(format!(
            "expected {EMBEDDING_DIM}-dim vector, got {}",
            v.len()
        ))
    })?;
    l2_normalize(&mut arr);
    Ok(arr)
}

pub(crate) fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn resolve_model(name: &str) -> Result<EmbeddingModel> {
    match name {
        DEFAULT_MODEL => Ok(EmbeddingModel::BGESmallENV15),
        ALT_MODEL => Ok(EmbeddingModel::AllMiniLML6V2),
        other => Err(HallouminateError::Config(format!(
            "unsupported embedding model {other:?}; choose {DEFAULT_MODEL:?} or {ALT_MODEL:?}"
        ))),
    }
}

pub(crate) fn check_or_set_model(conn: &DbConn, requested: &str) -> Result<()> {
    let stored: Option<String> = conn
        .raw()
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![META_KEY_MODEL],
            |row| row.get(0),
        )
        .optional()?;
    match stored {
        Some(existing) if existing != requested => Err(HallouminateError::Embed(format!(
            "embedding model mismatch: db has {existing:?}, requested {requested:?}; run `hallouminate index --reset` to rebuild"
        ))),
        Some(_) => Ok(()),
        None => {
            conn.raw().execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![META_KEY_MODEL, requested],
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::{apply_schema, open_db};

    fn fresh_conn() -> DbConn {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    #[test]
    fn l2_normalize_scales_3_4_vector_to_unit_norm() {
        let mut v = [3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6, "got {}", v[0]);
        assert!((v[1] - 0.8).abs() < 1e-6, "got {}", v[1]);
    }

    #[test]
    fn l2_normalize_already_unit_vector_is_unchanged() {
        let mut v = [1.0f32, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn l2_normalize_zero_vector_stays_zero() {
        let mut v = [0.0f32, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn l2_normalize_makes_arbitrary_vector_unit_length() {
        let mut v = [1.0f32, 2.0, 3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm = {norm}");
    }

    #[test]
    fn check_or_set_model_inserts_meta_row_when_empty() {
        let conn = fresh_conn();
        check_or_set_model(&conn, DEFAULT_MODEL).expect("first call");
        let stored: String = conn
            .raw()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_KEY_MODEL],
                |row| row.get(0),
            )
            .expect("meta row");
        assert_eq!(stored, DEFAULT_MODEL);
    }

    #[test]
    fn check_or_set_model_is_no_op_when_existing_matches() {
        let conn = fresh_conn();
        check_or_set_model(&conn, DEFAULT_MODEL).expect("first");
        check_or_set_model(&conn, DEFAULT_MODEL).expect("second must succeed");
        let count: i64 = conn
            .raw()
            .query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "must not insert duplicate meta rows");
    }

    #[test]
    fn check_or_set_model_errors_on_mismatch_pointing_at_reset() {
        let conn = fresh_conn();
        check_or_set_model(&conn, DEFAULT_MODEL).expect("first");
        let err = check_or_set_model(&conn, ALT_MODEL).expect_err("must mismatch");
        let msg = err.to_string();
        assert!(msg.contains(DEFAULT_MODEL), "{msg}");
        assert!(msg.contains(ALT_MODEL), "{msg}");
        assert!(msg.contains("--reset"), "{msg}");
    }

    #[test]
    fn resolve_model_accepts_default_and_alt() {
        assert!(matches!(
            resolve_model(DEFAULT_MODEL).unwrap(),
            EmbeddingModel::BGESmallENV15
        ));
        assert!(matches!(
            resolve_model(ALT_MODEL).unwrap(),
            EmbeddingModel::AllMiniLML6V2
        ));
    }

    #[test]
    fn resolve_model_rejects_unknown_name() {
        let err = resolve_model("clip-vit-b32").expect_err("must reject");
        assert!(err.to_string().contains("unsupported"), "{err}");
    }

    #[test]
    fn finalize_vector_rejects_wrong_dim() {
        let err = finalize_vector(vec![0.5; 100]).expect_err("must reject");
        assert!(err.to_string().contains("384-dim"), "{err}");
    }

    #[test]
    fn finalize_vector_normalizes_to_unit_length() {
        let mut input = vec![0.0f32; EMBEDDING_DIM];
        input[0] = 2.0;
        input[1] = 0.0;
        let arr = finalize_vector(input).expect("finalize");
        let norm: f32 = arr.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm = {norm}");
        assert!((arr[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    #[ignore = "downloads ~33MB model on first run; opt-in via --ignored"]
    fn embedder_produces_normalized_384d_vectors() {
        let conn = fresh_conn();
        let cache = std::env::temp_dir().join("hallouminate-embed-test-cache");
        let mut e = Embedder::try_new(DEFAULT_MODEL, &cache, &conn).expect("try_new");
        let out = e
            .embed_batch(&["the spice must flow".to_string()])
            .expect("embed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), EMBEDDING_DIM);
        let norm: f32 = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {norm}");
    }

    #[test]
    #[ignore = "downloads ~33MB model on first run; opt-in via --ignored"]
    fn embedder_is_deterministic_for_same_input() {
        let conn = fresh_conn();
        let cache = std::env::temp_dir().join("hallouminate-embed-test-cache");
        let mut e = Embedder::try_new(DEFAULT_MODEL, &cache, &conn).expect("try_new");
        let texts = vec!["witness me".to_string()];
        let a = e.embed_batch(&texts).expect("embed a");
        let b = e.embed_batch(&texts).expect("embed b");
        assert_eq!(a, b, "same input must yield bit-identical vectors");
    }
}
