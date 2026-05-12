use std::time::UNIX_EPOCH;

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::domain::common::{
    CorpusConfig, FileRef, HallouminateError, Mtime, Result, canonicalize_or_passthrough,
    expand_tilde,
};

pub fn scan(corpus: &CorpusConfig) -> Result<Vec<(FileRef, Mtime)>> {
    let include = build_globset(&corpus.globs)?;
    let exclude = build_globset(&corpus.exclude)?;
    let mut out = Vec::new();
    for raw in &corpus.paths {
        let root = expand_tilde(raw);
        walk_root(&root, include.as_ref(), exclude.as_ref(), &mut out)?;
    }
    Ok(out)
}

fn walk_root(
    root: &std::path::Path,
    include: Option<&GlobSet>,
    exclude: Option<&GlobSet>,
    out: &mut Vec<(FileRef, Mtime)>,
) -> Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| HallouminateError::Indexer(format!("walk error: {e}")))?;
        if let Some(hit) = visit_entry(&entry, include, exclude)? {
            out.push(hit);
        }
    }
    Ok(())
}

fn visit_entry(
    entry: &walkdir::DirEntry,
    include: Option<&GlobSet>,
    exclude: Option<&GlobSet>,
) -> Result<Option<(FileRef, Mtime)>> {
    if !entry.file_type().is_file() {
        return Ok(None);
    }
    let path = entry.path();
    if matches!(exclude, Some(ex) if ex.is_match(path)) {
        return Ok(None);
    }
    if matches!(include, Some(inc) if !inc.is_match(path)) {
        return Ok(None);
    }
    let mtime = entry_mtime_ms(entry)?;
    Ok(Some((canonicalize_or_passthrough(path), Mtime(mtime))))
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| HallouminateError::Config(format!("glob {pattern:?}: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| HallouminateError::Config(format!("globset build: {e}")))?;
    Ok(Some(set))
}

fn entry_mtime_ms(entry: &walkdir::DirEntry) -> Result<i64> {
    let meta = entry
        .metadata()
        .map_err(|e| HallouminateError::Indexer(format!("metadata: {e}")))?;
    let mtime = meta.modified()?;
    let dur = mtime.duration_since(UNIX_EPOCH).map_err(|_| {
        HallouminateError::Indexer(format!("pre-epoch mtime on {}", entry.path().display()))
    })?;
    Ok(dur.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn corpus_for(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "test".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec!["**/.git/**".into(), "**/node_modules/**".into()],
        }
    }

    fn file_names(scan_out: &[(FileRef, Mtime)]) -> Vec<String> {
        scan_out
            .iter()
            .map(|(f, _)| {
                f.as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn scan_returns_only_included_md_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("src/foo.md"), "the spice").unwrap();
        fs::write(root.join("src/bar.md"), "must flow").unwrap();
        fs::write(root.join("src/baz.txt"), "not markdown").unwrap();
        fs::write(root.join(".git/HEAD"), "ref: main").unwrap();
        fs::write(root.join("node_modules/x.md"), "vendored").unwrap();

        let result = scan(&corpus_for(root)).expect("scan");
        let names = file_names(&result);
        assert_eq!(result.len(), 2, "names = {names:?}");
        assert!(
            names.contains(&"foo.md".to_string()),
            "expected foo.md in {names:?}"
        );
        assert!(
            names.contains(&"bar.md".to_string()),
            "expected bar.md in {names:?}"
        );
    }

    #[test]
    fn scan_handles_single_file_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("CLAUDE.md");
        fs::write(&file, "single doc").unwrap();
        let corpus = CorpusConfig {
            name: "single".into(),
            paths: vec![file.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
        };
        let result = scan(&corpus).expect("scan");
        assert_eq!(result.len(), 1);
        assert_eq!(file_names(&result), vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn scan_with_empty_globs_matches_everything() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("a.md"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        let corpus = CorpusConfig {
            name: "all".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec![],
            exclude: vec![],
        };
        let result = scan(&corpus).expect("scan");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn scan_records_nonzero_mtime_for_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("doc.md");
        fs::write(&path, "content").unwrap();
        let corpus = CorpusConfig {
            name: "mtime".into(),
            paths: vec![path.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
        };
        let result = scan(&corpus).expect("scan");
        let (_, Mtime(ms)) = &result[0];
        assert!(
            *ms > 1_500_000_000_000,
            "expected post-2017 mtime, got {ms}"
        );
    }

    #[test]
    fn scan_invalid_glob_returns_config_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus = CorpusConfig {
            name: "bad".into(),
            paths: vec![tmp.path().to_string_lossy().into_owned()],
            globs: vec!["[invalid".into()],
            exclude: vec![],
        };
        let err = scan(&corpus).expect_err("invalid glob must fail");
        assert!(
            matches!(err, HallouminateError::Config(_)),
            "expected Config variant, got {err:?}"
        );
    }
}
