use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::domain::common::{HallouminateError, Result};

pub const EMBEDDING_DIM: usize = 384;
pub const DEFAULT_MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const E5_SMALL_MODEL: &str = "intfloat/multilingual-e5-small";
pub const ARCTIC_S_MODEL: &str = "snowflake/snowflake-arctic-embed-s";
pub const SUPPORTED_MODELS: [&str; 3] = [DEFAULT_MODEL, E5_SMALL_MODEL, ARCTIC_S_MODEL];

const LEGACY_DEFAULT_MODEL: &str = "bge-small-en-v1.5";

/// Whether a query or a passage is being embedded. Asymmetric retrieval
/// models (the e5 family) take different instruction prefixes for the two
/// sides; symmetric models (bge, arctic) prefix the query only. The
/// `Embedder` applies the per-model prefix in `embed_batch`, so callers only
/// say which side they are embedding: `Query` for the search string,
/// `Passage` for the chunks being indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedRole {
    Query,
    Passage,
}

pub trait EmbedBatch: Send {
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>>;
}

pub struct Embedder {
    inner: TextEmbedding,
    model_name: String,
}

impl Embedder {
    pub fn try_new(model_name: &str, quantized: bool, cache_dir: &Path) -> Result<Self> {
        let canonical_name = canonical_model_name(model_name)?;
        let model = resolve_model(canonical_name, quantized)?;
        let opts = TextInitOptions::new(model)
            .with_cache_dir(PathBuf::from(cache_dir))
            .with_show_download_progress(false);
        let inner = TextEmbedding::try_new(opts).map_err(|e| {
            HallouminateError::Embed(format!(
                "init {canonical_name}: {e}\n  \
                 hint: first run needs network to fetch the model into {}; \
                 run `hallouminate config download` to pre-warm the cache",
                cache_dir.display()
            ))
        })?;
        Ok(Self {
            inner,
            model_name: canonical_name.to_string(),
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

impl EmbedBatch for Embedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // fastembed v5 does NOT prepend any instruction prefix internally
        // (verified against fastembed 5.13.4: `TextEmbedding::embed`
        // tokenizes the raw input), so we apply the per-model prefix here.
        // Skip the allocation when the prefix is empty to keep the
        // symmetric-passage path (bge, arctic) free of needless clones.
        let prefix = instruction_prefix(&self.model_name, role);
        let raw = if prefix.is_empty() {
            self.inner.embed(texts, None)
        } else {
            let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
            self.inner.embed(&prefixed, None)
        }
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

/// Map a canonical model name + quantization flag to a fastembed enum
/// variant. Errors when a quantized variant is requested for a model that
/// has none (multilingual-e5-small ships only a full-precision ONNX). The
/// canonical name is guaranteed valid by `canonical_model_name`, so the
/// only fallible axis here is the missing-Q-variant case.
fn resolve_model(canonical: &str, quantized: bool) -> Result<EmbeddingModel> {
    let model = match (canonical, quantized) {
        (DEFAULT_MODEL, false) => EmbeddingModel::BGESmallENV15,
        (DEFAULT_MODEL, true) => EmbeddingModel::BGESmallENV15Q,
        (E5_SMALL_MODEL, false) => EmbeddingModel::MultilingualE5Small,
        (E5_SMALL_MODEL, true) => {
            return Err(HallouminateError::Config(format!(
                "{E5_SMALL_MODEL:?} has no quantized variant; \
                 set embeddings.quantized = false or choose {DEFAULT_MODEL:?} \
                 or {ARCTIC_S_MODEL:?}"
            )));
        }
        (ARCTIC_S_MODEL, false) => EmbeddingModel::SnowflakeArcticEmbedS,
        (ARCTIC_S_MODEL, true) => EmbeddingModel::SnowflakeArcticEmbedSQ,
        _ => unreachable!("resolve_model takes a canonical name from canonical_model_name"),
    };
    Ok(model)
}

/// Per-model instruction prefix for the given role. Asymmetric models (e5)
/// distinguish query vs passage; symmetric retrieval models (bge, arctic)
/// prefix the query side only and leave passages bare. Returns `""` when no
/// prefix applies. Trailing space is part of the prefix — it is prepended
/// verbatim to the text.
pub fn instruction_prefix(canonical: &str, role: EmbedRole) -> &'static str {
    match (canonical, role) {
        (DEFAULT_MODEL, EmbedRole::Query) | (ARCTIC_S_MODEL, EmbedRole::Query) => {
            "Represent this sentence for searching relevant passages: "
        }
        (DEFAULT_MODEL, EmbedRole::Passage) | (ARCTIC_S_MODEL, EmbedRole::Passage) => "",
        (E5_SMALL_MODEL, EmbedRole::Query) => "query: ",
        (E5_SMALL_MODEL, EmbedRole::Passage) => "passage: ",
        _ => "",
    }
}

pub fn canonical_model_name(name: &str) -> Result<&'static str> {
    match name {
        DEFAULT_MODEL | LEGACY_DEFAULT_MODEL => Ok(DEFAULT_MODEL),
        E5_SMALL_MODEL => Ok(E5_SMALL_MODEL),
        ARCTIC_S_MODEL => Ok(ARCTIC_S_MODEL),
        other => Err(HallouminateError::Config(format!(
            "unsupported embedding model {other:?}; choose one of \
             {DEFAULT_MODEL:?}, {E5_SMALL_MODEL:?}, {ARCTIC_S_MODEL:?} \
             (note: all-MiniLM-L6-v2 was dropped — delete the ground dir and \
             re-run `hallouminate index` after switching)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn resolve_model_maps_each_supported_model_full_precision() {
        assert!(matches!(
            resolve_model(DEFAULT_MODEL, false).unwrap(),
            EmbeddingModel::BGESmallENV15
        ));
        assert!(matches!(
            resolve_model(E5_SMALL_MODEL, false).unwrap(),
            EmbeddingModel::MultilingualE5Small
        ));
        assert!(matches!(
            resolve_model(ARCTIC_S_MODEL, false).unwrap(),
            EmbeddingModel::SnowflakeArcticEmbedS
        ));
    }

    #[test]
    fn resolve_model_picks_quantized_variant_when_one_exists() {
        assert!(matches!(
            resolve_model(DEFAULT_MODEL, true).unwrap(),
            EmbeddingModel::BGESmallENV15Q
        ));
        assert!(matches!(
            resolve_model(ARCTIC_S_MODEL, true).unwrap(),
            EmbeddingModel::SnowflakeArcticEmbedSQ
        ));
    }

    #[test]
    fn resolve_model_errors_for_quantized_e5_small_which_has_no_q_variant() {
        let err = resolve_model(E5_SMALL_MODEL, true)
            .expect_err("e5-small has no quantized ONNX; must error");
        let msg = err.to_string();
        assert!(msg.contains("no quantized variant"), "{msg}");
        assert!(msg.contains(E5_SMALL_MODEL), "{msg}");
    }

    #[test]
    fn instruction_prefix_is_asymmetric_for_e5_and_query_only_for_symmetric() {
        // e5: distinct query/passage prefixes.
        assert_eq!(
            instruction_prefix(E5_SMALL_MODEL, EmbedRole::Query),
            "query: "
        );
        assert_eq!(
            instruction_prefix(E5_SMALL_MODEL, EmbedRole::Passage),
            "passage: "
        );
        // bge + arctic: prefix the query, leave the passage bare.
        assert_eq!(
            instruction_prefix(DEFAULT_MODEL, EmbedRole::Query),
            "Represent this sentence for searching relevant passages: "
        );
        assert_eq!(instruction_prefix(DEFAULT_MODEL, EmbedRole::Passage), "");
        assert_eq!(
            instruction_prefix(ARCTIC_S_MODEL, EmbedRole::Query),
            "Represent this sentence for searching relevant passages: "
        );
        assert_eq!(instruction_prefix(ARCTIC_S_MODEL, EmbedRole::Passage), "");
    }

    #[test]
    fn canonical_model_name_accepts_legacy_default_alias_and_three_models() {
        assert_eq!(
            canonical_model_name("bge-small-en-v1.5").unwrap(),
            DEFAULT_MODEL
        );
        assert_eq!(canonical_model_name(DEFAULT_MODEL).unwrap(), DEFAULT_MODEL);
        assert_eq!(
            canonical_model_name(E5_SMALL_MODEL).unwrap(),
            E5_SMALL_MODEL
        );
        assert_eq!(
            canonical_model_name(ARCTIC_S_MODEL).unwrap(),
            ARCTIC_S_MODEL
        );
    }

    #[test]
    fn canonical_model_name_rejects_dropped_all_minilm_model() {
        // Breaking change: the old default-alternative is no longer in the
        // menu and must fail the same unsupported-model gate as junk input.
        for dropped in ["sentence-transformers/all-MiniLM-L6-v2", "all-minilm-l6-v2"] {
            let err =
                canonical_model_name(dropped).expect_err("dropped all-MiniLM model must error");
            assert!(
                err.to_string().contains("unsupported embedding model"),
                "{dropped}: {err}"
            );
        }
    }

    #[test]
    fn supported_model_names_are_hugging_face_repo_ids() {
        for model in SUPPORTED_MODELS {
            assert!(
                model.split_once('/').is_some(),
                "supported model must be a canonical HF repo id: {model}"
            );
        }
    }

    #[test]
    fn canonical_model_name_rejects_unknown_with_recovery_message() {
        let err = canonical_model_name("clip-vit-b32").expect_err("unsupported must error");
        let msg = err.to_string();
        assert!(msg.contains("unsupported embedding model"), "{msg}");
        assert!(msg.contains(DEFAULT_MODEL), "missing default option: {msg}");
        assert!(msg.contains(E5_SMALL_MODEL), "missing e5 option: {msg}");
        assert!(msg.contains(ARCTIC_S_MODEL), "missing arctic option: {msg}");
    }

    #[test]
    fn canonical_model_name_rejects_empty_string() {
        let err = canonical_model_name("").expect_err("empty name must error");
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
}
