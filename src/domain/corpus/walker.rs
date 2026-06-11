use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use ignore::gitignore::GitignoreBuilder;

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
        // "Auto-skip gitignored, unless explicitly included": if the corpus
        // root itself is gitignored by some ancestor `.gitignore`, the user
        // pointed at it on purpose — treat that as explicit opt-in and walk
        // it without applying gitignore filters. Otherwise honor `.gitignore`,
        // `.ignore`, `.git/info/exclude`, and the global gitignore as ripgrep
        // does.
        let explicit_opt_in = root_is_gitignored(&root);
        walk_root(
            &root,
            include.as_ref(),
            exclude.as_ref(),
            explicit_opt_in,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Corpus `paths` entries whose expanded root does not exist on disk.
///
/// A nonexistent root makes [`scan`] fail fatally (the underlying directory
/// walk yields an IO error on the first iteration). Callers that want to skip
/// a missing corpus rather than abort the whole run check this first; an empty
/// result means every root is present and `scan` is safe to call.
pub fn missing_roots(corpus: &CorpusConfig) -> Vec<PathBuf> {
    corpus
        .paths
        .iter()
        .map(|raw| expand_tilde(raw))
        .filter(|root| !root.exists())
        .collect()
}

fn walk_root(
    root: &Path,
    include: Option<&GlobSet>,
    exclude: Option<&GlobSet>,
    explicit_opt_in: bool,
    out: &mut Vec<(FileRef, Mtime)>,
) -> Result<()> {
    let mut builder = WalkBuilder::new(root);
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
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        // Prune ahead of include-match so caller-supplied excludes can mask
        // even paths the include glob would otherwise pull in.
        if let Some(ex) = exclude
            && ex.is_match(path)
        {
            continue;
        }
        if let Some(inc) = include
            && !inc.is_match(path)
        {
            continue;
        }
        let mtime = entry_mtime_ms(&entry)?;
        out.push((canonicalize_or_passthrough(path), Mtime(mtime)));
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
