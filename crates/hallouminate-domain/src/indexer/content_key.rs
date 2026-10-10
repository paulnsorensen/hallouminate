//! Content identity for the shared content rows of store schema 7.
//!
//! A content row holds the derived chunks of one file. Two files share one
//! content row only when every input of the derivation is equal. The key
//! covers these inputs:
//!
//! - the blake3 `content_hash` of the file bytes;
//! - the file name, because every format handler falls back to it for the
//!   summary, and the extension of the name selects the handler;
//! - the [`DerivationFingerprint`]: embedding model, quantization,
//!   embeddings mode, and store schema version;
//! - a digest of the derived output (summary, keywords, frontmatter, and
//!   every chunk field). This digest covers the handler, the chunker
//!   settings, and any future path-dependent field without a code change.
//!
//! The key never covers the directory part of the path, the root, the mtime,
//! or the index time. Those belong to the per-root file map.

use crate::common::file_name;
use crate::indexer::chunk::PreparedFile;

/// Whether the embedding model runs quantized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantization {
    Quantized,
    Full,
}

/// Whether the store computes embeddings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingsMode {
    Enabled,
    Disabled,
}

/// Store-level inputs of the derivation that the file does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationFingerprint(String);

impl DerivationFingerprint {
    /// Builds the fingerprint from the embedding identity and the schema version.
    pub fn new(
        embedding_model: &str,
        quantization: Quantization,
        embeddings: EmbeddingsMode,
        schema_version: u32,
    ) -> Self {
        let quantized = match quantization {
            Quantization::Quantized => true,
            Quantization::Full => false,
        };
        let embeddings_enabled = match embeddings {
            EmbeddingsMode::Enabled => true,
            EmbeddingsMode::Disabled => false,
        };
        Self(format!(
            "model={embedding_model};quantized={quantized};embeddings={embeddings_enabled};schema={schema_version}"
        ))
    }

    /// The canonical text of the fingerprint.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Hex identity of one shared content row group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentKey(String);

impl ContentKey {
    /// Derives the key of `file` under `fingerprint`.
    pub fn derive(file: &PreparedFile, fingerprint: &DerivationFingerprint) -> Self {
        let mut hasher = blake3::Hasher::new();
        put(&mut hasher, file.content_hash.as_bytes());
        put(&mut hasher, file_name(&file.file_ref).as_bytes());
        put(&mut hasher, fingerprint.as_str().as_bytes());
        put(&mut hasher, file.summary.as_bytes());
        put_count(&mut hasher, file.keywords.len());
        for keyword in &file.keywords {
            put(&mut hasher, keyword.as_bytes());
        }
        put_optional(&mut hasher, file.frontmatter.as_deref());
        put_count(&mut hasher, file.chunks.len());
        for chunk in &file.chunks {
            put_count(&mut hasher, chunk.ord);
            put_count(&mut hasher, chunk.heading_path.len());
            for heading in &chunk.heading_path {
                put(&mut hasher, heading.as_bytes());
            }
            put_count(&mut hasher, chunk.line_start);
            put_count(&mut hasher, chunk.line_end);
            put(&mut hasher, chunk.text.as_bytes());
            put(&mut hasher, chunk.search_text.as_bytes());
            put_optional(&mut hasher, chunk.claim_marks.as_deref());
            let structure = chunk
                .structure
                .as_ref()
                .and_then(|structure| serde_json::to_string(structure).ok());
            put_optional(&mut hasher, structure.as_deref());
            put_count(&mut hasher, chunk.overlap_bytes);
        }
        Self(hasher.finalize().to_hex().to_string())
    }

    /// Wraps hex text read back from a store column.
    pub fn from_stored(hex: &str) -> Self {
        Self(hex.to_string())
    }

    /// The hex text of the key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ContentKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

fn put(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn put_count(hasher: &mut blake3::Hasher, count: usize) {
    hasher.update(&(count as u64).to_le_bytes());
}

fn put_optional(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(text) => {
            hasher.update(&[1]);
            put(hasher, text.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::CorpusKey;
    use crate::indexer::chunk::PreparedChunk;

    fn fingerprint() -> DerivationFingerprint {
        DerivationFingerprint::new(
            "BAAI/bge-small-en-v1.5",
            Quantization::Full,
            EmbeddingsMode::Enabled,
            7,
        )
    }

    fn prepared(file_ref: &str, summary: &str, text: &str) -> PreparedFile {
        PreparedFile {
            file_ref: file_ref.to_string(),
            corpus_key: CorpusKey::from_configured_root("docs", "/tmp"),
            mtime_ms: 1,
            content_hash: "abc123".to_string(),
            summary: summary.to_string(),
            keywords: vec!["k".to_string()],
            frontmatter: None,
            indexed_at_ms: 1,
            chunks: vec![PreparedChunk {
                ord: 0,
                heading_path: Vec::new(),
                line_start: 1,
                line_end: 2,
                text: text.to_string(),
                search_text: format!("{summary} {text}"),
                claim_marks: None,
                structure: None,
                overlap_bytes: 0,
            }],
        }
    }

    #[test]
    fn content_key_is_equal_for_the_same_name_in_different_directories() {
        let first = prepared("/root-a/docs/a.md", "a.md", "body");
        let second = prepared("/root-b/other/a.md", "a.md", "body");
        assert_eq!(
            ContentKey::derive(&first, &fingerprint()),
            ContentKey::derive(&second, &fingerprint()),
        );
    }

    #[test]
    fn content_key_differs_for_identical_bytes_under_another_file_name() {
        let mut first = prepared("/root/a.md", "same", "body");
        let mut second = prepared("/root/b.md", "same", "body");
        first.content_hash = "same-bytes".to_string();
        second.content_hash = "same-bytes".to_string();
        assert_ne!(
            ContentKey::derive(&first, &fingerprint()),
            ContentKey::derive(&second, &fingerprint()),
        );
    }

    #[test]
    fn content_key_differs_for_another_derivation_fingerprint() {
        let file = prepared("/root/a.md", "a.md", "body");
        let model = "BAAI/bge-small-en-v1.5";
        let other_model = DerivationFingerprint::new(
            "other/model",
            Quantization::Full,
            EmbeddingsMode::Enabled,
            7,
        );
        let quantized =
            DerivationFingerprint::new(model, Quantization::Quantized, EmbeddingsMode::Enabled, 7);
        let embeddings_off =
            DerivationFingerprint::new(model, Quantization::Full, EmbeddingsMode::Disabled, 7);
        let other_schema =
            DerivationFingerprint::new(model, Quantization::Full, EmbeddingsMode::Enabled, 8);
        let base = ContentKey::derive(&file, &fingerprint());
        assert_ne!(base, ContentKey::derive(&file, &other_model));
        assert_ne!(base, ContentKey::derive(&file, &quantized));
        assert_ne!(base, ContentKey::derive(&file, &embeddings_off));
        assert_ne!(base, ContentKey::derive(&file, &other_schema));
    }

    #[test]
    fn content_key_differs_when_the_chunker_output_differs() {
        let first = prepared("/root/a.md", "a.md", "body");
        let mut second = first.clone();
        second.chunks[0].line_end = 3;
        assert_ne!(
            ContentKey::derive(&first, &fingerprint()),
            ContentKey::derive(&second, &fingerprint()),
        );
    }

    #[test]
    fn content_key_ignores_mtime_and_index_time() {
        let first = prepared("/root/a.md", "a.md", "body");
        let mut second = first.clone();
        second.mtime_ms = 99;
        second.indexed_at_ms = 99;
        assert_eq!(
            ContentKey::derive(&first, &fingerprint()),
            ContentKey::derive(&second, &fingerprint()),
        );
    }
}
