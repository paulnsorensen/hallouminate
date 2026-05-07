use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(pub i64);

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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Corpus(pub String);

impl Corpus {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HallouminateError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("embed: {0}")]
    Embed(String),

    #[error("config: {0}")]
    Config(String),

    #[error("indexer: {0}")]
    Indexer(String),
}

pub type Result<T> = std::result::Result<T, HallouminateError>;
