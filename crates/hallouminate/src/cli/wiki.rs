//! `hallouminate wiki status` — list files under the repository's hallouminate
//! wiki and corpus roots that carry uncommitted git changes (worktree
//! modifications not yet staged, plus untracked files).
//!
//! This is a read-only, local command in the mold of `config show`: it resolves
//! the effective config for `cwd`, takes every corpus root that lives inside the
//! enclosing git working tree, and reports each root's dirty files. It never
//! touches the daemon — the answer comes from `git status` and the filesystem.
//!
//! Git detection is delegated to the `git` binary (the same choice
//! `resolve_hooks_dir` makes) rather than a `git2`/`gix` dependency: the command
//! is a single read-only `git status --porcelain` call, and shelling out keeps
//! worktree / submodule / `core.*` semantics identical to what the user's git
//! would compute.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};

use hallouminate_domain::common::expand_tilde;

#[derive(Debug, Default)]
pub struct WikiStatusArgs {
    /// Baseline (`--config PATH`) override, mirroring `config show`.
    pub config: Option<PathBuf>,
    /// Working directory for repo-config discovery and git detection. `None`
    /// resolves to `std::env::current_dir()` at command time.
    pub cwd: Option<PathBuf>,
    /// Emit machine-readable JSON instead of the grouped text view.
    pub json: bool,
}

/// One dirty file the command reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirtyFile {
    /// Owning corpus name (e.g. `repo:oslo:wiki`).
    pub corpus: String,
    /// Path relative to the git working-tree root.
    pub path: String,
    /// Two-character porcelain XY status code (e.g. ` M`, `??`, `MM`).
    pub status: String,
}

#[derive(Debug, serde::Serialize)]
struct WikiStatusReport {
    git_root: String,
    files: Vec<DirtyFile>,
    count: usize,
}

pub fn cmd_wiki_status(args: WikiStatusArgs) -> anyhow::Result<()> {
    let cwd = match &args.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("read current working directory")?,
    };

    // Git-repo-only: resolve the working-tree root first so a non-repo cwd
    // fails with a clear message before any config work.
    let git_root = git_toplevel(&cwd)?;

    let baseline = hallouminate_config::load_xdg(args.config.as_deref())?;
    let (effective, _layers) =
        hallouminate_config::resolve_for_cwd(&baseline, &cwd, args.config.as_deref())
            .map_err(|e| anyhow!("resolve hallouminate config for {}: {e}", cwd.display()))?;
    let corpora = effective
        .effective_corpora()
        .map_err(|e| anyhow!("derive effective corpora: {e}"))?;

    // Map every in-tree corpus root to its path relative to the git root. Roots
    // outside the working tree are dropped — git cannot report on them and
    // passing them as pathspecs would error.
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    for corpus in &corpora {
        for raw in &corpus.paths {
            let abs = canonicalize_partial(&expand_tilde(raw));
            if let Ok(rel) = abs.strip_prefix(&git_root) {
                roots.push((corpus.name.clone(), rel.to_path_buf()));
            }
        }
    }

    let files = if roots.is_empty() {
        Vec::new()
    } else {
        let entries = git_status(&git_root, &roots)?;
        assign_files(&entries, &roots)
    };

    if args.json {
        let report = WikiStatusReport {
            git_root: git_root.to_string_lossy().into_owned(),
            count: files.len(),
            files,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&git_root, &files);
    }
    Ok(())
}

/// One parsed porcelain record: its XY status code and its path relative to the
/// git working-tree root.
struct StatusEntry {
    status: String,
    path: PathBuf,
}

/// Resolve the git working-tree root for `cwd`, or fail if `cwd` is not inside
/// a git repository. Mirrors `resolve_hooks_dir`'s decision to defer to the
/// `git` binary so worktrees and submodules resolve exactly as git would.
fn git_toplevel(cwd: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("invoke `git -C {} rev-parse`", cwd.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "not a git repository: {} ({})",
            cwd.display(),
            stderr.trim()
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("git rev-parse output not UTF-8")?
        .trim();
    // git prints the physical (symlink-resolved) toplevel; canonicalize the
    // corpus roots the same way so prefix comparison stays in one namespace.
    Ok(canonicalize_partial(Path::new(stdout)))
}

/// Run `git status --porcelain -z` scoped to the given repo-relative roots and
/// return the worktree-dirty and untracked entries. Staged-only changes (a
/// clean worktree column) are excluded — this command answers "what have I not
/// committed yet in the wiki", and staged files are already captured.
fn git_status(git_root: &Path, roots: &[(String, PathBuf)]) -> anyhow::Result<Vec<StatusEntry>> {
    // Deduplicate pathspecs: several corpora can share a root.
    let mut pathspecs: Vec<&Path> = roots.iter().map(|(_, rel)| rel.as_path()).collect();
    pathspecs.sort();
    pathspecs.dedup();

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(git_root)
        .args(["status", "--porcelain", "-z", "--untracked-files=all", "--"])
        .args(&pathspecs);
    let output = cmd
        .output()
        .with_context(|| format!("invoke `git -C {} status`", git_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git status failed: {}", stderr.trim()));
    }
    parse_porcelain_z(&output.stdout)
}

/// Parse `git status --porcelain -z` output.
///
/// Records are NUL-terminated. Each record is `XY <path>`; a rename/copy record
/// (X is `R` or `C`) carries a second NUL-separated path (the original), which
/// must be consumed to keep the stream aligned even though such records are
/// staged (clean worktree column) and thus filtered out. A record is kept when
/// its worktree column (Y) is non-blank — that covers ` M`, `MM`, `??`
/// (untracked), and unmerged states — i.e. everything with an uncommitted
/// change in the working tree.
fn parse_porcelain_z(stdout: &[u8]) -> anyhow::Result<Vec<StatusEntry>> {
    let text = std::str::from_utf8(stdout).context("git status output not UTF-8")?;
    let mut tokens = text.split('\0');
    let mut out = Vec::new();
    while let Some(record) = tokens.next() {
        if record.is_empty() {
            continue;
        }
        let bytes = record.as_bytes();
        if bytes.len() < 3 {
            return Err(anyhow!("malformed git status record: {record:?}"));
        }
        let x = bytes[0];
        let y = bytes[1];
        let status = record[..2].to_string();
        let path = &record[3..];
        // A rename/copy record has a trailing original-path token; consume it.
        if x == b'R' || x == b'C' {
            tokens.next();
        }
        if y != b' ' {
            out.push(StatusEntry {
                status,
                path: PathBuf::from(path),
            });
        }
    }
    Ok(out)
}

/// Assign each dirty entry to the deepest corpus root that contains it. An entry
/// matching no root (should not happen, since git is scoped to the roots) is
/// dropped defensively.
fn assign_files(entries: &[StatusEntry], roots: &[(String, PathBuf)]) -> Vec<DirtyFile> {
    let mut files = Vec::new();
    for entry in entries {
        let best = roots
            .iter()
            .filter(|(_, rel)| entry.path.starts_with(rel))
            .max_by_key(|(_, rel)| rel.components().count());
        if let Some((corpus, _)) = best {
            files.push(DirtyFile {
                corpus: corpus.clone(),
                path: entry.path.to_string_lossy().into_owned(),
                status: entry.status.clone(),
            });
        }
    }
    files
}

fn print_text(git_root: &Path, files: &[DirtyFile]) {
    if files.is_empty() {
        println!("No unstaged changes under wiki or corpus roots.");
        return;
    }
    println!("{}", git_root.display());
    // Group by corpus, preserving a stable (sorted) corpus order.
    let mut by_corpus: BTreeMap<&str, Vec<&DirtyFile>> = BTreeMap::new();
    for file in files {
        by_corpus.entry(&file.corpus).or_default().push(file);
    }
    for (corpus, mut group) in by_corpus {
        group.sort_by(|a, b| a.path.cmp(&b.path));
        println!("{corpus}");
        for file in group {
            println!("  {} {}", file.status, file.path);
        }
    }
    let noun = if files.len() == 1 { "file" } else { "files" };
    println!("\n{} {} with unstaged changes", files.len(), noun);
}

/// Canonicalize the longest existing prefix of `path` and re-append the missing
/// tail. Unlike `std::fs::canonicalize`, this succeeds for a not-yet-created
/// wiki directory while still resolving `.`/`..`/symlinks in the part that does
/// exist — so the result compares cleanly against git's physical toplevel.
fn canonicalize_partial(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            // `ancestor` came from `path.ancestors()`, so this strip never fails.
            let tail = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            // Joining an empty tail would append a trailing separator; the
            // fully-existing path (its own first ancestor) needs no tail.
            if tail.as_os_str().is_empty() {
                return canonical;
            }
            return canonical.join(tail);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A git repo seeded as a hallouminate tenant with an indexed wiki file.
    fn seed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "t@t"]);
        run_git(root, &["config", "user.name", "t"]);
        crate::cli::cmd_init_repo(crate::cli::InitRepoArgs {
            name: "demo".into(),
            path: Some(root.to_path_buf()),
            force: false,
        })
        .expect("init-repo");
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-qm", "seed"]);
        dir
    }

    fn status(root: &Path) -> Vec<DirtyFile> {
        let cwd = root.to_path_buf();
        let git_root = git_toplevel(&cwd).expect("toplevel");
        let baseline = hallouminate_config::Config::default();
        let (effective, _) =
            hallouminate_config::resolve_for_cwd(&baseline, &cwd, None).expect("resolve");
        let corpora = effective.effective_corpora().expect("corpora");
        let mut roots: Vec<(String, PathBuf)> = Vec::new();
        for corpus in &corpora {
            for raw in &corpus.paths {
                let abs = canonicalize_partial(&expand_tilde(raw));
                if let Ok(rel) = abs.strip_prefix(&git_root) {
                    roots.push((corpus.name.clone(), rel.to_path_buf()));
                }
            }
        }
        let entries = git_status(&git_root, &roots).expect("git status");
        assign_files(&entries, &roots)
    }

    #[test]
    fn clean_wiki_reports_no_files() {
        let dir = seed_repo();
        assert!(
            status(dir.path()).is_empty(),
            "clean tree => no dirty files"
        );
    }

    #[test]
    fn untracked_wiki_file_is_reported() {
        let dir = seed_repo();
        fs::write(dir.path().join(".hallouminate/wiki/new.md"), "# new\n").expect("write");
        let files = status(dir.path());
        assert_eq!(files.len(), 1, "one untracked file: {files:?}");
        assert_eq!(files[0].corpus, "repo:demo:wiki");
        assert_eq!(files[0].path, ".hallouminate/wiki/new.md");
        assert_eq!(files[0].status, "??");
    }

    #[test]
    fn modified_unstaged_wiki_file_is_reported() {
        let dir = seed_repo();
        fs::write(
            dir.path().join(".hallouminate/wiki/index.md"),
            "# changed\n",
        )
        .expect("write");
        let files = status(dir.path());
        assert_eq!(files.len(), 1, "one modified file: {files:?}");
        assert_eq!(files[0].path, ".hallouminate/wiki/index.md");
        assert_eq!(files[0].status, " M", "worktree-modified, not staged");
    }

    #[test]
    fn staged_only_wiki_file_is_excluded() {
        let dir = seed_repo();
        fs::write(
            dir.path().join(".hallouminate/wiki/index.md"),
            "# changed\n",
        )
        .expect("write");
        run_git(dir.path(), &["add", ".hallouminate/wiki/index.md"]);
        assert!(
            status(dir.path()).is_empty(),
            "a fully-staged change has a clean worktree column and must be excluded"
        );
    }

    #[test]
    fn changes_outside_wiki_and_corpus_roots_are_ignored() {
        let dir = seed_repo();
        fs::write(dir.path().join("README.md"), "outside\n").expect("write");
        assert!(
            status(dir.path()).is_empty(),
            "a change outside every corpus root must not be reported"
        );
    }

    #[test]
    fn declared_corpus_root_changes_are_reported() {
        let dir = seed_repo();
        // Re-seed config with a source corpus so a non-wiki root participates.
        let cfg = dir.path().join(".hallouminate/config.toml");
        fs::write(
            &cfg,
            "[[repository]]\nname = \"demo\"\npath = \".\"\ncorpus_paths = [\"docs\"]\n",
        )
        .expect("rewrite config");
        fs::create_dir_all(dir.path().join("docs")).expect("mkdir docs");
        fs::write(dir.path().join("docs/api.md"), "# api\n").expect("write");
        let files = status(dir.path());
        assert_eq!(files.len(), 1, "one corpus file: {files:?}");
        assert_eq!(files[0].corpus, "repo:demo:corpus");
        assert_eq!(files[0].path, "docs/api.md");
    }

    #[test]
    fn non_git_directory_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = git_toplevel(dir.path()).expect_err("must fail outside a git repo");
        assert!(
            err.to_string().contains("not a git repository"),
            "err: {err}"
        );
    }

    #[test]
    fn parse_consumes_rename_original_path_token() {
        // `R  new\0old\0 M dirty\0` — the staged rename (clean worktree column)
        // is filtered, but its original-path token must be consumed so the
        // following ` M dirty` record still parses.
        let raw = b"R  new\0old\0 M dirty\0";
        let entries = parse_porcelain_z(raw).expect("parse");
        assert_eq!(entries.len(), 1, "only the worktree-dirty record survives");
        assert_eq!(entries[0].path, PathBuf::from("dirty"));
        assert_eq!(entries[0].status, " M");
    }
}
