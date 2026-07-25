use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use ignore::gitignore::GitignoreBuilder;

use crate::common::{
    CorpusConfig, CorpusKey, FileRef, HallouminateError, Mtime, Result, expand_tilde,
};

/// One scanned file paired with the canonical corpus root that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// Root-aware corpus identity selected during scanning.
    pub corpus_key: CorpusKey,
    /// Canonical path of the file on disk.
    pub file: FileRef,
    /// Current file modification time.
    pub mtime: Mtime,
}

#[derive(Debug)]
struct ScanRoot {
    corpus_key: CorpusKey,
    configured_path: PathBuf,
}

fn configured_roots(corpus: &CorpusConfig) -> Vec<ScanRoot> {
    let mut roots: Vec<ScanRoot> = Vec::new();
    for configured_root in &corpus.paths {
        let corpus_key = CorpusKey::from_configured_root(&corpus.name, configured_root);
        let mut duplicate = false;
        for root in &roots {
            if root.corpus_key == corpus_key {
                duplicate = true;
                break;
            }
        }
        if duplicate {
            continue;
        }
        roots.push(ScanRoot {
            corpus_key,
            configured_path: expand_tilde(configured_root),
        });
    }
    roots
}

fn owning_root<'a>(file: &Path, roots: &'a [ScanRoot]) -> Option<&'a ScanRoot> {
    let mut owner: Option<&ScanRoot> = None;
    for root in roots {
        if !file.starts_with(&root.corpus_key.canonical_root) {
            continue;
        }
        let specificity = root.corpus_key.canonical_root.components().count();
        match owner {
            None => owner = Some(root),
            Some(current) => {
                let current_specificity = current.corpus_key.canonical_root.components().count();
                if specificity > current_specificity {
                    owner = Some(root);
                }
            }
        }
    }
    owner
}
pub fn scan(corpus: &CorpusConfig) -> Result<Vec<ScannedFile>> {
    let include = build_globset(&corpus.globs)?;
    let exclude = build_globset(&corpus.exclude)?;
    let roots = configured_roots(corpus);
    let mut out = Vec::new();
    for root in &roots {
        // "Auto-skip gitignored, unless explicitly included": if the corpus
        // root itself is gitignored by some ancestor `.gitignore`, the user
        // pointed at it on purpose — treat that as explicit opt-in and walk
        // it without applying gitignore filters. Otherwise honor `.gitignore`,
        // `.ignore`, `.git/info/exclude`, and the global gitignore as ripgrep
        // does.
        let explicit_opt_in = root_is_gitignored(&root.configured_path);
        walk_root(
            root,
            &roots,
            include.as_ref(),
            exclude.as_ref(),
            explicit_opt_in,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Corpus `paths` entries whose expanded root is confirmed absent on disk.
///
/// A nonexistent root makes [`scan`] fail fatally (the underlying directory
/// walk yields an IO error on the first iteration). Callers that want to skip
/// a missing corpus rather than abort the whole run check this first; an empty
/// result means every root is present and `scan` is safe to call.
///
/// Only `try_exists() == Ok(false)` counts as missing. A root that errors on
/// the existence probe (e.g. permission denied on a parent component) is *not*
/// reported here, so the real IO error still surfaces through `scan`/`walk_root`
/// instead of being masked as a misleading "does not exist" skip.
pub fn missing_roots(corpus: &CorpusConfig) -> Vec<PathBuf> {
    corpus
        .paths
        .iter()
        .map(|raw| expand_tilde(raw))
        .filter(|root| matches!(root.try_exists(), Ok(false)))
        .collect()
}

fn walk_root(
    root: &ScanRoot,
    roots: &[ScanRoot],
    include: Option<&GlobSet>,
    exclude: Option<&GlobSet>,
    explicit_opt_in: bool,
    out: &mut Vec<ScannedFile>,
) -> Result<()> {
    let mut builder = WalkBuilder::new(&root.configured_path);
    builder
        .standard_filters(true)
        // Dotfiles are content too — only skip them when gitignore says so.
        .hidden(false)
        .follow_links(false);
    if explicit_opt_in {
        builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }
    for entry in builder.build() {
        let entry = entry.map_err(|e| HallouminateError::Indexer(format!("walk error: {e}")))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        // Prune ahead of include-match so caller-supplied excludes can mask
        // even paths the include glob would otherwise pull in.
        if let Some(exclude) = exclude
            && exclude.is_match(path)
        {
            continue;
        }
        if let Some(include) = include
            && !include.is_match(path)
        {
            continue;
        }
        let file = crate::common::canonicalize_or_passthrough(path);
        let Some(owner) = owning_root(file.as_path(), roots) else {
            continue;
        };
        if owner.corpus_key != root.corpus_key {
            continue;
        }
        let mtime = Mtime(entry_mtime_ms(&entry)?);
        out.push(ScannedFile {
            corpus_key: root.corpus_key.clone(),
            file,
            mtime,
        });
    }
    Ok(())
}

/// Walks up from `root` looking for a `.git` boundary, collecting every
/// `.gitignore` along the way, then asks "would git consider this path
/// ignored?". Returns false on any structural surprise (no repo found,
/// gitignore parse error, etc.) so the default behavior is to honor
/// `.gitignore` rather than silently bypass it.
fn root_is_gitignored(root: &Path) -> bool {
    let mut repo_root: Option<PathBuf> = None;
    let mut gitignore_files: Vec<PathBuf> = Vec::new();
    let mut cursor: Option<&Path> = root.parent();
    while let Some(c) = cursor {
        let gi = c.join(".gitignore");
        if gi.is_file() {
            gitignore_files.push(gi);
        }
        if c.join(".git").exists() {
            repo_root = Some(c.to_path_buf());
            break;
        }
        cursor = c.parent();
    }
    let Some(repo_root) = repo_root else {
        return false;
    };
    let mut builder = GitignoreBuilder::new(&repo_root);
    // Outer-to-inner: ancestor patterns apply first; inner `.gitignore` files
    // override them. We collected innermost-first, so reverse.
    for gi in gitignore_files.iter().rev() {
        // `GitignoreBuilder::add` returns `Some(_)` for non-fatal partial
        // errors (a single malformed glob line); per the `ignore` crate
        // docs, every other valid glob in the file is still added. Treating
        // that as fatal would silently disengage the opt-in escape hatch
        // whenever an ancestor `.gitignore` (including the user's global
        // gitignore) has even one bad line, so drop the partial error and
        // keep going rather than bail.
        let _ = builder.add(gi);
    }
    let Ok(gitignore) = builder.build() else {
        return false;
    };
    gitignore
        .matched_path_or_any_parents(root, root.is_dir())
        .is_ignore()
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

fn entry_mtime_ms(entry: &ignore::DirEntry) -> Result<i64> {
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
            global: false,
        }
    }

    fn file_names(scan_out: &[ScannedFile]) -> Vec<String> {
        let mut names = Vec::with_capacity(scan_out.len());
        for scanned in scan_out {
            names.push(
                scanned
                    .file
                    .as_path()
                    .file_name()
                    .expect("scanned file name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        names
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
            global: false,
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
            global: false,
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
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let Mtime(ms) = result[0].mtime;
        assert!(ms > 1_500_000_000_000, "expected post-2017 mtime, got {ms}");
    }

    #[test]
    fn scan_assigns_overlapping_files_to_the_longest_canonical_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path();
        let child = parent.join("nested");
        fs::create_dir_all(&child).expect("create child root");
        fs::write(parent.join("parent.md"), "parent").expect("write parent file");
        fs::write(child.join("child.md"), "child").expect("write child file");
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![
                parent.to_string_lossy().into_owned(),
                child.to_string_lossy().into_owned(),
                child.to_string_lossy().into_owned(),
            ],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };

        let scanned = scan(&corpus).expect("scan overlapping roots");
        assert_eq!(scanned.len(), 2, "identical roots must deduplicate");
        let parent_root = std::fs::canonicalize(parent).expect("canonical parent");
        let child_root = std::fs::canonicalize(&child).expect("canonical child");
        for file in scanned {
            let name = file
                .file
                .as_path()
                .file_name()
                .expect("file name")
                .to_string_lossy();
            match name.as_ref() {
                "parent.md" => assert_eq!(file.corpus_key.canonical_root, parent_root),
                "child.md" => assert_eq!(file.corpus_key.canonical_root, child_root),
                other => panic!("unexpected scanned file {other}"),
            }
        }
    }

    #[test]
    fn scan_invalid_glob_returns_config_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus = CorpusConfig {
            name: "bad".into(),
            paths: vec![tmp.path().to_string_lossy().into_owned()],
            globs: vec!["[invalid".into()],
            exclude: vec![],
            global: false,
        };
        let err = scan(&corpus).expect_err("invalid glob must fail");
        let msg = err.to_string();
        assert!(
            matches!(err, HallouminateError::Config(_)),
            "expected Config variant, got {err:?}"
        );
        assert!(
            msg.contains("[invalid"),
            "error message should name the offending pattern, got: {msg}"
        );
        assert!(
            msg.starts_with("config: glob"),
            "error message should identify the source as a glob config error, got: {msg}"
        );
    }

    #[test]
    fn excluded_directory_is_not_descended_into() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("excluded_dir")).unwrap();
        // A .md file inside the excluded dir that would match the include glob.
        fs::write(root.join("excluded_dir/keepme.md"), "should not appear").unwrap();
        // A file outside the excluded dir to confirm the walker still works.
        fs::write(root.join("visible.md"), "should appear").unwrap();
        let corpus = CorpusConfig {
            name: "prune".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec!["**/excluded_dir/**".into()],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let names = file_names(&result);
        assert_eq!(result.len(), 1, "names = {names:?}");
        assert!(
            names.contains(&"visible.md".to_string()),
            "expected visible.md in {names:?}"
        );
        assert!(
            !names.contains(&"keepme.md".to_string()),
            "keepme.md inside excluded_dir should not be visited, got {names:?}"
        );
    }

    #[test]
    fn scan_skips_gitignored_files_by_default() {
        // A corpus rooted at a git repo respects `.gitignore` without any
        // explicit exclude glob — gitignored files are filtered automatically.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "secret.md\nbuild/\n").unwrap();
        fs::write(root.join("keep.md"), "ok").unwrap();
        fs::write(root.join("secret.md"), "ignored").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.md"), "built").unwrap();

        let corpus = CorpusConfig {
            name: "gi".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let names = file_names(&result);
        assert!(
            names.contains(&"keep.md".to_string()),
            "keep.md should be indexed: {names:?}"
        );
        assert!(
            !names.contains(&"secret.md".to_string()),
            "secret.md must be filtered by .gitignore: {names:?}"
        );
        assert!(
            !names.contains(&"out.md".to_string()),
            "build/out.md must be filtered by .gitignore: {names:?}"
        );
    }

    #[test]
    fn scan_indexes_gitignored_root_when_explicitly_chosen() {
        // The "explicit opt-in" escape hatch: if the corpus root itself is
        // gitignored, the user pointed at it on purpose — don't second-guess
        // them by re-applying gitignore inside the chosen subtree.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "secrets/\n").unwrap();
        fs::create_dir_all(root.join("secrets")).unwrap();
        fs::write(root.join("secrets/diary.md"), "private").unwrap();
        fs::write(root.join("secrets/notes.md"), "more").unwrap();

        let corpus = CorpusConfig {
            name: "opt-in".into(),
            paths: vec![root.join("secrets").to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let names = file_names(&result);
        assert!(
            names.contains(&"diary.md".to_string()),
            "diary.md must be indexed — gitignored root counts as explicit opt-in: {names:?}"
        );
        assert!(
            names.contains(&"notes.md".to_string()),
            "notes.md must be indexed — gitignored root counts as explicit opt-in: {names:?}"
        );
    }

    #[test]
    fn root_is_gitignored_distinguishes_opt_in_from_default_paths() {
        // Verify both branches of the opt-in detector directly. The previous
        // scan-level test asserted an outcome that was identical between the
        // two branches it claimed to discriminate, so it couldn't catch a
        // regression in the dichotomy. This one can.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "secrets/\n").unwrap();
        let secrets = root.join("secrets");
        fs::create_dir_all(&secrets).unwrap();
        let normal = root.join("src");
        fs::create_dir_all(&normal).unwrap();

        assert!(
            root_is_gitignored(&secrets),
            "secrets/ is gitignored — must be detected as explicit opt-in"
        );
        assert!(
            !root_is_gitignored(&normal),
            "src/ is not gitignored — must not trigger opt-in"
        );
        assert!(
            !root_is_gitignored(root),
            "repo root itself is not gitignored — must not trigger opt-in"
        );
    }

    #[test]
    fn root_is_gitignored_returns_false_when_no_git_ancestor() {
        // No `.git` boundary above the tempdir — the helper must bail with
        // `false` so the walk falls back to honoring gitignore by default
        // rather than silently disabling it.
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            !root_is_gitignored(tmp.path()),
            "no .git ancestor must yield false"
        );
    }

    #[test]
    fn root_is_gitignored_survives_malformed_ancestor_gitignore() {
        // Regression guard for the partial-add fix: a single malformed glob
        // line in an ancestor `.gitignore` used to make `root_is_gitignored`
        // bail with `false`, silently disengaging the opt-in escape hatch.
        // The fix drops the partial-add error, so valid globs after the bad
        // line still apply and a gitignored corpus root is still detected.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        // First line is a malformed character class; second line is valid.
        fs::write(root.join(".gitignore"), "[invalid\nsecrets/\n").unwrap();
        let secrets = root.join("secrets");
        fs::create_dir_all(&secrets).unwrap();

        assert!(
            root_is_gitignored(&secrets),
            "valid `secrets/` rule must still apply despite the malformed line above it"
        );
    }
}
