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

/// One file surfaced by a directory walk, resolved to the corpus root that
/// owns it and the path relative to that root — the target every include and
/// exclude glob matches against.
struct OwnedEntry {
    entry: ignore::DirEntry,
    file: PathBuf,
    relative: PathBuf,
}

/// The directory that include and exclude patterns anchor to for `root`.
///
/// A configured root usually names a directory, so patterns anchor to it. A
/// root may instead name one file, as `paths = [".../CLAUDE.md"]` does.
/// Patterns then anchor to that file's parent, so a pattern sees the file's
/// own name instead of an empty path.
fn match_base(root: &ScanRoot) -> &Path {
    let canonical = root.corpus_key.canonical_root.as_path();
    if !canonical.is_file() {
        return canonical;
    }
    let Some(parent) = canonical.parent() else {
        return canonical;
    };
    parent
}

/// Walk `root.configured_path`, keeping only files owned by `root` itself
/// (deeper overlapping roots claim their own descendants on their own walk).
/// Applies traversal filters (gitignore, opt-in) but not include/exclude —
/// callers match those against `OwnedEntry::relative`.
fn walk_owned_entries(
    root: &ScanRoot,
    roots: &[ScanRoot],
    explicit_opt_in: bool,
) -> Result<Vec<OwnedEntry>> {
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
    let base = match_base(root);
    let mut out = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(|e| HallouminateError::Indexer(format!("walk error: {e}")))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let file = crate::common::canonicalize_or_passthrough(entry.path()).into_path_buf();
        let Some(owner) = owning_root(&file, roots) else {
            continue;
        };
        if owner.corpus_key != root.corpus_key {
            continue;
        }
        let Ok(relative) = file.strip_prefix(base) else {
            continue;
        };
        let relative = relative.to_path_buf();
        out.push(OwnedEntry {
            entry,
            file,
            relative,
        });
    }
    Ok(out)
}

fn walk_root(
    root: &ScanRoot,
    roots: &[ScanRoot],
    include: Option<&GlobSet>,
    exclude: Option<&GlobSet>,
    explicit_opt_in: bool,
    out: &mut Vec<ScannedFile>,
) -> Result<()> {
    for owned in walk_owned_entries(root, roots, explicit_opt_in)? {
        // Include first, then exclude — exclude still wins over include for
        // the kept set, but this ordering matches `selection_warnings`'s
        // counting rule (include-eligibility gates exclude counting).
        if let Some(include) = include
            && !include.is_match(&owned.relative)
        {
            continue;
        }
        if let Some(exclude) = exclude
            && exclude.is_match(&owned.relative)
        {
            continue;
        }
        let mtime = Mtime(entry_mtime_ms(&owned.entry)?);
        out.push(ScannedFile {
            corpus_key: root.corpus_key.clone(),
            file: owned.file.into(),
            mtime,
        });
    }
    Ok(())
}

/// Rule kind a [`SelectionWarning`] reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// A `corpus.globs` entry.
    Include,
    /// A `corpus.exclude` entry.
    Exclude,
}

/// A configured include/exclude pattern that matched zero files under one
/// corpus root, surfaced as an advisory by `config validate` and
/// `corpus_stats` — never as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionWarning {
    /// Configured corpus name.
    pub corpus: String,
    /// Canonical root the pattern was evaluated against.
    pub root: PathBuf,
    /// Whether `pattern` came from `globs` or `exclude`.
    pub kind: RuleKind,
    /// The original pattern string, as configured.
    pub pattern: String,
}

impl std::fmt::Display for SelectionWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            RuleKind::Include => "include",
            RuleKind::Exclude => "exclude",
        };
        write!(
            f,
            "corpus {:?} root {}: {kind} pattern {:?} matched no files",
            self.corpus,
            self.root.display(),
            self.pattern,
        )
    }
}

/// Report zero-match include/exclude patterns for every configured root of
/// `corpus`. Roots reported by [`missing_roots`] are skipped — a missing root
/// keeps its own diagnostic and must not produce an invented zero-match
/// count. A valid, empty corpus still succeeds with warnings; only malformed
/// or absolute patterns are errors.
///
/// Counting follows the contract in `.cheese/specs/workspace-path-contract.md`:
/// include matches are counted after traversal filters but before
/// include/exclude filtering; exclude matches are counted among
/// include-eligible files, before applying any exclude rule. One rule can
/// never hide another rule's matches.
pub fn selection_warnings(corpus: &CorpusConfig) -> Result<Vec<SelectionWarning>> {
    let missing = missing_roots(corpus);
    let roots = configured_roots(corpus);
    let include = compile_indexed_globs(&corpus.globs)?;
    let exclude = compile_indexed_globs(&corpus.exclude)?;
    let mut warnings = Vec::new();
    for root in &roots {
        if missing.contains(&root.configured_path) {
            continue;
        }
        let explicit_opt_in = root_is_gitignored(&root.configured_path);
        let owned = walk_owned_entries(root, &roots, explicit_opt_in)?;
        let mut include_counts = vec![0usize; include.patterns.len()];
        let mut exclude_counts = vec![0usize; exclude.patterns.len()];
        for entry in &owned {
            if let Some(set) = &include.set {
                for idx in set.matches(&entry.relative) {
                    include_counts[idx] += 1;
                }
            }
            let include_eligible = include
                .set
                .as_ref()
                .is_none_or(|set| set.is_match(&entry.relative));
            if !include_eligible {
                continue;
            }
            if let Some(set) = &exclude.set {
                for idx in set.matches(&entry.relative) {
                    exclude_counts[idx] += 1;
                }
            }
        }
        for (pattern, count) in include.patterns.iter().zip(include_counts) {
            if count == 0 {
                warnings.push(SelectionWarning {
                    corpus: corpus.name.clone(),
                    root: root.corpus_key.canonical_root.clone(),
                    kind: RuleKind::Include,
                    pattern: pattern.clone(),
                });
            }
        }
        for (pattern, count) in exclude.patterns.iter().zip(exclude_counts) {
            if count == 0 {
                warnings.push(SelectionWarning {
                    corpus: corpus.name.clone(),
                    root: root.corpus_key.canonical_root.clone(),
                    kind: RuleKind::Exclude,
                    pattern: pattern.clone(),
                });
            }
        }
    }
    Ok(warnings)
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

/// Compiled patterns paired with their original strings, indexed identically
/// to `GlobSet::matches`'s returned indices — so a zero-match warning can
/// name the exact configured pattern.
struct IndexedGlobs {
    set: Option<GlobSet>,
    patterns: Vec<String>,
}

fn validate_relative_pattern(pattern: &str) -> Result<()> {
    if Path::new(pattern).is_absolute() {
        return Err(HallouminateError::Config(format!(
            "glob {pattern:?} must be root-relative (matched against the corpus root), \
             e.g. \"docs/**/*.md\" not \"/docs/**/*.md\""
        )));
    }
    Ok(())
}

fn compile_indexed_globs(patterns: &[String]) -> Result<IndexedGlobs> {
    if patterns.is_empty() {
        return Ok(IndexedGlobs {
            set: None,
            patterns: Vec::new(),
        });
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        validate_relative_pattern(pattern)?;
        let glob = Glob::new(pattern)
            .map_err(|e| HallouminateError::Config(format!("glob {pattern:?}: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| HallouminateError::Config(format!("globset build: {e}")))?;
    Ok(IndexedGlobs {
        set: Some(set),
        patterns: patterns.to_vec(),
    })
}

pub(crate) fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    Ok(compile_indexed_globs(patterns)?.set)
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
    fn scan_anchors_patterns_to_the_file_name_for_a_single_file_root() {
        // A `paths` entry may name one file rather than a directory. The
        // relative match target is then the file's own name, so a pattern
        // still has something to match. A directory-anchored pattern must
        // not match, because the root is the file itself.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("docs").join("CLAUDE.md");
        fs::create_dir_all(file.parent().expect("parent")).unwrap();
        fs::write(&file, "single doc").unwrap();
        let corpus_with = |glob: &str| CorpusConfig {
            name: "file-root".into(),
            paths: vec![file.to_string_lossy().into_owned()],
            globs: vec![glob.into()],
            exclude: vec![],
            global: false,
        };

        let by_name = scan(&corpus_with("CLAUDE.md")).expect("scan by name");
        assert_eq!(
            file_names(&by_name),
            vec!["CLAUDE.md".to_string()],
            "a bare file name must match a single-file root"
        );

        let by_directory = scan(&corpus_with("docs/**/*.md")).expect("scan by directory");
        assert!(
            by_directory.is_empty(),
            "the root is the file itself, so a directory-anchored pattern must not match: {by_directory:?}"
        );
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
    fn scan_matches_root_anchored_include_relative_to_corpus_root() {
        // AC-5 regression: `docs/**/*.md` under `paths=["."]` must select
        // `docs/start.md` and NOT `libs/docs/start.md` — the pattern anchors
        // to the corpus root, not to any `docs/` segment anywhere below it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("libs/docs")).unwrap();
        fs::write(root.join("docs/start.md"), "root doc").unwrap();
        fs::write(root.join("libs/docs/start.md"), "nested doc").unwrap();
        let corpus = CorpusConfig {
            name: "anchored".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["docs/**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let relative: Vec<PathBuf> = result
            .iter()
            .map(|f| {
                f.file
                    .as_path()
                    .strip_prefix(std::fs::canonicalize(root).unwrap())
                    .unwrap()
                    .to_path_buf()
            })
            .collect();
        assert_eq!(
            relative,
            vec![PathBuf::from("docs/start.md")],
            "expected only the root-level docs/start.md, got {relative:?}"
        );
    }

    #[test]
    fn scan_excludes_root_anchored_pattern_relative_to_corpus_root() {
        // AC-5 regression: `warden/**/README.md` must drop root-level
        // `warden/README.md` but keep `infra/modules/warden/README.md`, which
        // sits under a different (non-root) `warden/` directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("warden")).unwrap();
        fs::create_dir_all(root.join("infra/modules/warden")).unwrap();
        fs::write(root.join("warden/README.md"), "root warden").unwrap();
        fs::write(root.join("infra/modules/warden/README.md"), "nested warden").unwrap();
        let corpus = CorpusConfig {
            name: "anchored-exclude".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec!["warden/**/README.md".into()],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        let relative: Vec<PathBuf> = result
            .iter()
            .map(|f| {
                f.file
                    .as_path()
                    .strip_prefix(std::fs::canonicalize(root).unwrap())
                    .unwrap()
                    .to_path_buf()
            })
            .collect();
        assert_eq!(
            relative,
            vec![PathBuf::from("infra/modules/warden/README.md")],
            "expected only the nested warden README, got {relative:?}"
        );
    }

    #[test]
    fn scan_drops_file_matching_both_include_and_exclude() {
        // Pins that reordering include-then-exclude in `walk_root` did not
        // change the kept set: exclude must still win when a file matches
        // both an include and an exclude pattern.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/start.md"), "doc").unwrap();
        let corpus = CorpusConfig {
            name: "both".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["docs/**/*.md".into()],
            exclude: vec!["docs/**/*.md".into()],
            global: false,
        };
        let result = scan(&corpus).expect("scan");
        assert!(
            result.is_empty(),
            "exclude must win when a file matches both include and exclude: {result:?}"
        );
    }

    #[test]
    fn build_globset_rejects_absolute_pattern() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus = CorpusConfig {
            name: "abs".into(),
            paths: vec![tmp.path().to_string_lossy().into_owned()],
            globs: vec!["/docs/**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let err = scan(&corpus).expect_err("absolute glob must be rejected");
        let msg = err.to_string();
        assert!(
            matches!(err, HallouminateError::Config(_)),
            "expected Config variant, got {err:?}"
        );
        assert!(
            msg.contains("root-relative") && msg.contains("docs/**/*.md"),
            "error should show a root-relative example, got: {msg}"
        );
    }

    #[test]
    fn selection_warnings_reports_zero_match_include_and_exclude_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("a.md"), "a").unwrap();
        let corpus = CorpusConfig {
            name: "warn".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into(), "docs/**/*.md".into()],
            exclude: vec!["drafts/**".into()],
            global: false,
        };
        let warnings = selection_warnings(&corpus).expect("selection_warnings");
        assert_eq!(warnings.len(), 2, "warnings = {warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.kind == RuleKind::Include && w.pattern == "docs/**/*.md"),
            "expected a zero-match include warning naming the original pattern: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.kind == RuleKind::Exclude && w.pattern == "drafts/**"),
            "expected a zero-match exclude warning naming the original pattern: {warnings:?}"
        );
    }

    #[test]
    fn selection_warnings_counts_each_exclude_independently() {
        // Two excludes both matching the same file must each report a
        // non-zero count — one rule cannot hide another rule's matches.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("drafts")).unwrap();
        fs::write(root.join("drafts/wip.md"), "wip").unwrap();
        let corpus = CorpusConfig {
            name: "double-exclude".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec![],
            exclude: vec!["drafts/**".into(), "**/*.md".into()],
            global: false,
        };
        let warnings = selection_warnings(&corpus).expect("selection_warnings");
        assert!(
            warnings.is_empty(),
            "both excludes matched the same file — neither should warn: {warnings:?}"
        );
    }

    #[test]
    fn selection_warnings_skips_missing_roots_without_inventing_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let corpus = CorpusConfig {
            name: "missing".into(),
            paths: vec![missing.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let warnings = selection_warnings(&corpus).expect("selection_warnings");
        assert!(
            warnings.is_empty(),
            "a missing root must not produce an invented zero-match count: {warnings:?}"
        );
    }

    #[test]
    fn selection_warnings_succeeds_for_valid_empty_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus = CorpusConfig {
            name: "empty".into(),
            paths: vec![tmp.path().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let warnings = selection_warnings(&corpus).expect("empty corpus must still succeed");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, RuleKind::Include);
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
