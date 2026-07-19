use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub corpora: Vec<CorpusReport>,
    /// Corpora skipped during the run, one human-readable line each (e.g. a
    /// missing root). Empty in the common all-healthy case, so it is omitted
    /// from the JSON rather than serialized as `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusReport {
    pub name: String,
    pub files_upserted: usize,
    pub files_touched: usize,
    pub files_deleted: usize,
    pub files_skipped_empty: usize,
    #[serde(default)]
    pub files_skipped_unreadable: usize,
    pub chunks_inserted: usize,
    pub embeddings_inserted: usize,
}
