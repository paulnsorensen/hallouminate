use crate::domain::common::{HallouminateError, Result};

/// BAAI bge small model: a small, English, symmetric retrieval model.
pub const BGE_SMALL_MODEL: &str = "BAAI/bge-small-en-v1.5";
/// Multilingual, asymmetric retrieval model (distinct query/passage prefixes).
pub const E5_SMALL_MODEL: &str = "intfloat/multilingual-e5-small";
/// Snowflake Arctic small model; symmetric retrieval like [`BGE_SMALL_MODEL`].
pub const ARCTIC_S_MODEL: &str = "snowflake/snowflake-arctic-embed-s";
/// The default embedding model — the one [`crate::app::config`] selects when a
/// config omits `embeddings.model`. Named separately from the model it points
/// at so the default can move without renaming a model const (and so the
/// const name no longer lies about which model is actually the default).
pub const DEFAULT_EMBED_MODEL: &str = ARCTIC_S_MODEL;
/// Every model name accepted by [`canonical_model_name`], for menus and tests.
pub const SUPPORTED_MODELS: [&str; 3] = [BGE_SMALL_MODEL, E5_SMALL_MODEL, ARCTIC_S_MODEL];

/// Returns [`HallouminateError::Config`] when `name` is not one of the
/// supported models, with a message listing the valid choices. The bare
/// `bge-small-en-v1.5` alias is no longer accepted — use the full
/// [`BGE_SMALL_MODEL`] id.
pub fn canonical_model_name(name: &str) -> Result<&'static str> {
    match name {
        BGE_SMALL_MODEL => Ok(BGE_SMALL_MODEL),
        E5_SMALL_MODEL => Ok(E5_SMALL_MODEL),
        ARCTIC_S_MODEL => Ok(ARCTIC_S_MODEL),
        other => Err(HallouminateError::Config(format!(
            "unsupported embedding model {other:?}; choose one of \
             {BGE_SMALL_MODEL:?}, {E5_SMALL_MODEL:?}, {ARCTIC_S_MODEL:?} \
             (note: all-MiniLM-L6-v2 was dropped — delete the ground dir and \
             re-run `hallouminate index` after switching)"
        ))),
    }
}

// TEMPORARY (Stage 2b bridge, removed in Stage 2c): downstream callers still
// reach the embedding mechanism through the domain path.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_model_name_accepts_the_three_full_model_ids() {
        assert_eq!(
            canonical_model_name(BGE_SMALL_MODEL).unwrap(),
            BGE_SMALL_MODEL
        );
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
    fn canonical_model_name_rejects_dropped_bare_bge_alias() {
        // The bare `bge-small-en-v1.5` alias was torched (pre-1.0, nobody
        // depends on it). It must now fail the same unsupported-model gate as
        // any junk input; the full `BAAI/bge-small-en-v1.5` id stays valid.
        let err = canonical_model_name("bge-small-en-v1.5")
            .expect_err("bare bge alias must no longer resolve");
        let msg = err.to_string();
        assert!(msg.contains("unsupported embedding model"), "{msg}");
        // The full id is still the recovery hint.
        assert!(msg.contains(BGE_SMALL_MODEL), "{msg}");
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
        assert!(msg.contains(BGE_SMALL_MODEL), "missing bge option: {msg}");
        assert!(msg.contains(E5_SMALL_MODEL), "missing e5 option: {msg}");
        assert!(msg.contains(ARCTIC_S_MODEL), "missing arctic option: {msg}");
    }

    #[test]
    fn canonical_model_name_rejects_empty_string() {
        let err = canonical_model_name("").expect_err("empty name must error");
        assert!(err.to_string().contains("unsupported"), "{err}");
    }

    /// Doc-pin: the README model table is the user-facing menu, and it must
    /// not drift from the code's `SUPPORTED_MODELS` / `DEFAULT_EMBED_MODEL`.
    /// Fails CI if a model is added/renamed in code without updating the
    /// table, or if the table stops marking the real default as the default.
    #[test]
    fn readme_model_table_lists_every_supported_model_and_marks_the_default() {
        const README: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
        for model in SUPPORTED_MODELS {
            assert!(
                README.contains(model),
                "README model table is missing supported model {model:?}; \
                 update the table in README.md so the docs don't drift from code"
            );
        }
        // The default must be named on the same line as a "default" marker so
        // a reader (and this test) can tell which model is selected when
        // `embeddings.model` is omitted.
        let marks_default = README.lines().any(|line| {
            line.contains(DEFAULT_EMBED_MODEL) && line.to_ascii_lowercase().contains("default")
        });
        assert!(
            marks_default,
            "README must mark {DEFAULT_EMBED_MODEL:?} as the default on its table row"
        );
    }
}
