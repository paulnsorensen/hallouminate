use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod paths;

pub use paths::{RetiredRoot, canonicalize_or_passthrough, expand_tilde, retired_roots};

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

/// Identifies one configured corpus root after filesystem canonicalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CorpusKey {
    /// Configured corpus name.
    pub name: String,
    /// Canonical filesystem root owned by this corpus identity.
    pub canonical_root: PathBuf,
}

impl CorpusKey {
    /// Creates an identity from a configured corpus name and root.
    ///
    /// # Examples
    ///
    /// ```
    /// use hallouminate_domain::common::CorpusKey;
    ///
    /// let key = CorpusKey::from_configured_root("docs", "/tmp");
    /// assert_eq!(key.name, "docs");
    /// ```
    pub fn from_configured_root(name: impl Into<String>, configured_root: &str) -> Self {
        let root = expand_tilde(configured_root);
        let canonical_root = canonicalize_or_passthrough(&root).into_path_buf();
        Self {
            name: name.into(),
            canonical_root,
        }
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

impl CorpusConfig {
    /// Returns the distinct root-aware identities in configured order.
    ///
    /// # Examples
    ///
    /// ```
    /// use hallouminate_domain::common::CorpusConfig;
    ///
    /// let config = CorpusConfig {
    ///     name: "docs".into(),
    ///     paths: vec!["/tmp/docs".into(), "/tmp/docs".into()],
    ///     ..Default::default()
    /// };
    /// assert_eq!(config.corpus_keys().len(), 1);
    /// ```
    pub fn corpus_keys(&self) -> Vec<CorpusKey> {
        let mut keys = Vec::new();
        for path in &self.paths {
            let key = CorpusKey::from_configured_root(&self.name, path);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// Returns the first configured root identity.
    pub fn primary_corpus_key(&self) -> Option<CorpusKey> {
        self.corpus_keys().into_iter().next()
    }

    /// Returns the most specific configured root that owns `path`.
    pub fn corpus_key_for_path(&self, path: &Path) -> Option<CorpusKey> {
        let path = canonicalize_or_passthrough(path);
        self.corpus_keys()
            .into_iter()
            .filter(|key| path.as_path().starts_with(&key.canonical_root))
            .max_by_key(|key| key.canonical_root.components().count())
    }
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
