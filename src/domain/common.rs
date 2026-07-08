use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod paths;

pub use paths::{canonicalize_or_passthrough, expand_tilde};

/// A reference to a file on disk, identified by its path.
///
/// Wraps a [`PathBuf`] to give file paths a distinct domain type so they are
/// not confused with arbitrary strings or paths as they flow through indexing,
/// storage, and search.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileRef(PathBuf);

impl FileRef {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for FileRef {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<PathBuf> for FileRef {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}
/// A file's modification time, in milliseconds since the Unix epoch.
///
/// Used to detect whether an on-disk file has changed since it was last
/// indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mtime(pub i64);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusConfig {
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub globs: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Marks this corpus as the single global corpus. This is a uniqueness
    /// marker only — config validation rejects more than one such corpus.
    #[serde(default)]
    pub global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Corpus(String);

impl Corpus {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The crate-wide error type, covering every fallible operation in
/// hallouminate.
#[derive(Debug, thiserror::Error)]
pub enum HallouminateError {
    /// A filesystem operation failed. Produced automatically (via `#[from]`)
    /// whenever an [`std::io::Error`] propagates — reading config, walking
    /// corpus paths, or accessing the ground store.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The vector store (LanceDB) failed — opening, reading, or applying a
    /// write batch to the on-disk index.
    #[error("db: {0}")]
    Db(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Embedding generation failed — loading the embedding model or encoding
    /// chunk text into vectors.
    #[error("embed: {0}")]
    Embed(String),

    /// Configuration was invalid — a malformed config file, a rejected corpus
    /// or repository entry, or a name that violates the `repo:` namespace
    /// rules.
    #[error("config: {0}")]
    Config(String),

    /// Indexing failed while chunking files or applying batches to the store.
    #[error("indexer: {0}")]
    Indexer(String),

    /// A lexical search backend (ripgrep) failed with a real error —
    /// not "no matches" (rg exit 1), but rg exiting with status >= 2 or
    /// terminating abnormally (e.g. by signal).
    #[error("search: {0}")]
    Search(String),

    /// The on-disk store was written at an OLDER schema version than this build
    /// expects; the daemon-open path rebuilds it from source.
    #[error("store schema stale: found v{found}, expected v{expected} at {}", ground_dir.display())]
    StoreSchemaStale {
        found: u32,
        expected: u32,
        ground_dir: PathBuf,
    },
}

/// Crate-wide result alias, fixing the error type to [`HallouminateError`].
pub type Result<T> = std::result::Result<T, HallouminateError>;
