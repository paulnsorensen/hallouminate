use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use hallouminate_domain::common::{HallouminateError, Result};
use hallouminate_domain::embeddings::{
    ARCTIC_S_MODEL, BGE_SMALL_MODEL, E5_SMALL_MODEL, canonical_model_name,
};

/// Output dimensionality shared by every supported model. All three embed to
/// 384-dim vectors, so the rest of the pipeline can use a fixed-size array.
pub const EMBEDDING_DIM: usize = 384;
/// Bounded fastembed batch size for ONNX runs. Passing `None` selects
/// fastembed's internal default (256 sequences × 512 tokens), which sets the
/// ORT CPU arena's high-water mark in the multi-GB range and is never
/// reclaimed afterwards (see `.hallouminate/wiki/ort-arena-retention.md`).
/// `Some(32)` is the measured, verified mitigation.
const EMBED_BATCH_SIZE: usize = 32;

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

/// Embeds a batch of texts into unit-normalized vectors. Implemented by
/// [`Embedder`] and by test doubles; the `role` selects the query- vs
/// passage-side instruction prefix for asymmetric models.
pub trait EmbedBatch: Send {
    /// Embed `texts` under `role`, returning one vector per input in order.
    ///
    /// # Errors
    /// Returns [`HallouminateError::Embed`] if the backend fails to embed or
    /// returns a vector whose dimensionality is not [`EMBEDDING_DIM`].
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>>;
}

/// Whether to load the quantized (`*Q`) ONNX variant of a model. Internal to
/// model resolution; the wire/config boundary stays a `bool`, mapped here once
/// in [`Embedder::try_new`] so [`resolve_model`] matches on an exhaustive enum
/// rather than a bare flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quantization {
    Full,
    Quantized,
}

impl From<bool> for Quantization {
    fn from(quantized: bool) -> Self {
        if quantized {
            Self::Quantized
        } else {
            Self::Full
        }
    }
}

pub struct Embedder {
    inner: TextEmbedding,
    model_name: String,
}

impl Embedder {
    pub fn try_new(model_name: &str, quantized: bool, cache_dir: &Path) -> Result<Self> {
        let canonical_name = canonical_model_name(model_name)?;
        let model = resolve_model(canonical_name, quantized.into())?;
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
            self.inner.embed(texts, Some(EMBED_BATCH_SIZE))
        } else {
            let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
            self.inner.embed(&prefixed, Some(EMBED_BATCH_SIZE))
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

/// Map a canonical model name + [`Quantization`] to a fastembed enum
/// variant. Errors when [`Quantization::Quantized`] is requested for a model
/// that has none (multilingual-e5-small ships only a full-precision ONNX). The
/// canonical name is guaranteed valid by [`canonical_model_name`], so the
/// only fallible axis here is the missing-Q-variant case.
fn resolve_model(canonical: &str, quantization: Quantization) -> Result<EmbeddingModel> {
    use Quantization::{Full, Quantized};
    let model = match (canonical, quantization) {
        (BGE_SMALL_MODEL, Full) => EmbeddingModel::BGESmallENV15,
        (BGE_SMALL_MODEL, Quantized) => EmbeddingModel::BGESmallENV15Q,
        (E5_SMALL_MODEL, Full) => EmbeddingModel::MultilingualE5Small,
        (E5_SMALL_MODEL, Quantized) => {
            return Err(HallouminateError::Config(format!(
                "{E5_SMALL_MODEL:?} has no quantized variant; \
                 set embeddings.quantized = false or choose {BGE_SMALL_MODEL:?} \
                 or {ARCTIC_S_MODEL:?}"
            )));
        }
        (ARCTIC_S_MODEL, Full) => EmbeddingModel::SnowflakeArcticEmbedS,
        (ARCTIC_S_MODEL, Quantized) => EmbeddingModel::SnowflakeArcticEmbedSQ,
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
        (BGE_SMALL_MODEL, EmbedRole::Query) | (ARCTIC_S_MODEL, EmbedRole::Query) => {
            "Represent this sentence for searching relevant passages: "
        }
        (BGE_SMALL_MODEL, EmbedRole::Passage) | (ARCTIC_S_MODEL, EmbedRole::Passage) => "",
        (E5_SMALL_MODEL, EmbedRole::Query) => "query: ",
        (E5_SMALL_MODEL, EmbedRole::Passage) => "passage: ",
        _ => "",
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
            resolve_model(BGE_SMALL_MODEL, Quantization::Full).unwrap(),
            EmbeddingModel::BGESmallENV15
        ));
        assert!(matches!(
            resolve_model(E5_SMALL_MODEL, Quantization::Full).unwrap(),
            EmbeddingModel::MultilingualE5Small
        ));
        assert!(matches!(
            resolve_model(ARCTIC_S_MODEL, Quantization::Full).unwrap(),
            EmbeddingModel::SnowflakeArcticEmbedS
        ));
    }

    #[test]
    fn resolve_model_picks_quantized_variant_when_one_exists() {
        assert!(matches!(
            resolve_model(BGE_SMALL_MODEL, Quantization::Quantized).unwrap(),
            EmbeddingModel::BGESmallENV15Q
        ));
        assert!(matches!(
            resolve_model(ARCTIC_S_MODEL, Quantization::Quantized).unwrap(),
            EmbeddingModel::SnowflakeArcticEmbedSQ
        ));
    }

    #[test]
    fn quantization_from_bool_maps_true_to_quantized_false_to_full() {
        assert_eq!(Quantization::from(true), Quantization::Quantized);
        assert_eq!(Quantization::from(false), Quantization::Full);
    }

    #[test]
    fn resolve_model_errors_for_quantized_e5_small_which_has_no_q_variant() {
        let err = resolve_model(E5_SMALL_MODEL, Quantization::Quantized)
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
            instruction_prefix(BGE_SMALL_MODEL, EmbedRole::Query),
            "Represent this sentence for searching relevant passages: "
        );
        assert_eq!(instruction_prefix(BGE_SMALL_MODEL, EmbedRole::Passage), "");
        assert_eq!(
            instruction_prefix(ARCTIC_S_MODEL, EmbedRole::Query),
            "Represent this sentence for searching relevant passages: "
        );
        assert_eq!(instruction_prefix(ARCTIC_S_MODEL, EmbedRole::Passage), "");
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
