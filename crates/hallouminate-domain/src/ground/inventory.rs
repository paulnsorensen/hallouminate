use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::common::{CorpusKey, FileRef};

/// What the store reports about one root's indexed files.
#[derive(Debug, Clone, Default)]
pub enum IndexedFiles {
    /// Files that have indexed rows under the root.
    Known(Arc<HashSet<FileRef>>),
    /// The store cannot list its indexed files.
    #[default]
    Unknown,
    /// The listing failed with this reason.
    ListingFailed(String),
}

/// One root's file sets for one request.
///
/// `eligible` holds the files the indexer scan selects. `indexed` holds the
/// files that already have indexed rows. The lexical fallback searches the
/// difference.
#[derive(Debug, Clone, Default)]
pub struct RootInventory {
    pub eligible: Arc<HashSet<FileRef>>,
    pub indexed: IndexedFiles,
}

/// The per-root file sets of one request, keyed by corpus identity.
pub type Inventory = BTreeMap<CorpusKey, RootInventory>;
