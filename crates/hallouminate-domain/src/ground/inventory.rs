use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::common::{CorpusConfig, CorpusKey, FileRef};
use crate::corpus::ScannedFile;
use crate::search::{ChunkRetrieval, RootHandle, RootIndex};

/// What the store reports about one root's indexed files.
#[derive(Debug, Clone, Default)]
pub enum IndexedFiles {
    /// Files that have indexed rows under the root, with the store's handle
    /// on the same read.
    Known(RootIndex),
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

impl RootInventory {
    /// The store handle from the root's single file-map read, if any.
    pub fn handle(&self) -> Option<&RootHandle> {
        match &self.indexed {
            IndexedFiles::Known(RootIndex { handle, .. }) => handle.as_ref(),
            IndexedFiles::Unknown => None,
            IndexedFiles::ListingFailed(_) => None,
        }
    }
}

/// Lists the indexed files of each root of `corpus`, one store read per root.
pub async fn list_root_indexes(
    corpus: &CorpusConfig,
    store: &dyn ChunkRetrieval,
) -> Vec<(CorpusKey, IndexedFiles)> {
    let mut listed = Vec::new();
    for corpus_key in corpus.corpus_keys() {
        let indexed = match store.load_root_index(&corpus_key).await {
            Ok(Some(index)) => IndexedFiles::Known(index),
            Ok(None) => IndexedFiles::Unknown,
            Err(error) => IndexedFiles::ListingFailed(error.to_string()),
        };
        listed.push((corpus_key, indexed));
    }
    listed
}

/// Pairs each listed root with the scanned `files` that belong to it. A root
/// that no scanned file belongs to gets an empty `eligible` set.
pub fn assemble_inventory(
    listed: Vec<(CorpusKey, IndexedFiles)>,
    files: &[ScannedFile],
) -> Vec<(CorpusKey, RootInventory)> {
    let mut eligible: BTreeMap<CorpusKey, HashSet<FileRef>> = BTreeMap::new();
    for scanned in files {
        eligible
            .entry(scanned.corpus_key.clone())
            .or_default()
            .insert(scanned.file.clone());
    }
    let mut roots = Vec::with_capacity(listed.len());
    for (corpus_key, indexed) in listed {
        let files = eligible.remove(&corpus_key).unwrap_or_default();
        roots.push((
            corpus_key,
            RootInventory {
                eligible: Arc::new(files),
                indexed,
            },
        ));
    }
    roots
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::common::Mtime;

    fn scanned(key: &CorpusKey, path: &str) -> ScannedFile {
        ScannedFile {
            corpus_key: key.clone(),
            file: FileRef::new(PathBuf::from(path)),
            mtime: Mtime(0),
        }
    }

    #[test]
    fn assemble_inventory_routes_each_scanned_file_to_its_own_root() {
        let root_a = CorpusKey::from_configured_root("docs", "/a");
        let root_b = CorpusKey::from_configured_root("docs", "/b");
        let root_c = CorpusKey::from_configured_root("docs", "/c");
        let listed = vec![
            (root_a.clone(), IndexedFiles::Unknown),
            (
                root_b.clone(),
                IndexedFiles::ListingFailed("boom".to_string()),
            ),
            (root_c.clone(), IndexedFiles::Unknown),
        ];
        let files = vec![
            scanned(&root_a, "/a/one.md"),
            scanned(&root_a, "/a/two.md"),
            scanned(&root_b, "/b/three.md"),
        ];

        let roots = assemble_inventory(listed, &files);

        assert_eq!(roots.len(), 3);
        let [(key_a, inv_a), (key_b, inv_b), (key_c, inv_c)] = &roots[..] else {
            panic!("three roots expected: {roots:?}");
        };
        assert_eq!((key_a, key_b, key_c), (&root_a, &root_b, &root_c));
        assert_eq!(inv_a.eligible.len(), 2);
        assert!(
            inv_a
                .eligible
                .contains(&FileRef::new(PathBuf::from("/a/one.md")))
        );
        assert_eq!(inv_b.eligible.len(), 1);
        assert!(
            inv_b
                .eligible
                .contains(&FileRef::new(PathBuf::from("/b/three.md")))
        );
        assert!(inv_c.eligible.is_empty(), "an unmatched root gets no files");
        let IndexedFiles::ListingFailed(reason) = &inv_b.indexed else {
            panic!("the listing state must carry through: {:?}", inv_b.indexed);
        };
        assert_eq!(reason, "boom");
    }
}
