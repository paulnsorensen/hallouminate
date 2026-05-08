use std::path::{Path, PathBuf};

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
    Db(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error("embed: {0}")]
    Embed(String),

    #[error("config: {0}")]
    Config(String),

    #[error("indexer: {0}")]
    Indexer(String),
}

pub type Result<T> = std::result::Result<T, HallouminateError>;
