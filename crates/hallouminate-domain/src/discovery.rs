//! Bounded downward discovery of sub-repo `.hallouminate` roots.
//!
//! When a read/search request arrives from a directory **above all repos**
//! (baseline-only mode, post-#102), the config layer walks downward from that
//! directory to find every sub-repo wiki and unions them with the
//! baseline-registered ones (#106). This module owns the walk.
//!
//! The walk is deliberately bounded: it honors `.gitignore`, skips hidden /
//! dot directories (except `.hallouminate` itself), and stops at a max depth.
//! It never scans above the root, so a `cd ~/Dev && ground "..."` searches
//! the repos under `~/Dev` without ever touching `~` or the rest of the
//! filesystem.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::repository::WIKI_RELATIVE_PATH;

/// Default cap on how deep the downward walk descends below the root, in
/// directory levels. `~/Dev/<repo>/.hallouminate` sits two levels under
/// `~/Dev`, so a small cap reaches the common `parent-dir/repo/.hallouminate`
/// layout while refusing to walk arbitrarily deep nested trees.
pub const DEFAULT_MAX_DEPTH: usize = 4;

/// Filters applied to the downward walk.
///
/// `respect_gitignore` honors `.gitignore`, `.ignore`, `.git/info/exclude`,
/// and the global gitignore (as ripgrep does). `skip_hidden` prunes dot
/// directories — `.hallouminate` is always traversed regardless, since it is
/// the very thing being discovered.
#[derive(Debug, Clone, Copy)]
pub struct IgnoreRules {
    pub respect_gitignore: bool,
    pub skip_hidden: bool,
}

impl Default for IgnoreRules {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            skip_hidden: true,
        }
    }
}

/// A sub-repo wiki found by the downward walk.
///
/// `repo_root` is the directory that owns the `.hallouminate/` dir (the
/// parent the user would `cd` into). `config_path` is its
/// `.hallouminate/config.toml` when present; `wiki_dir` is its
/// `.hallouminate/wiki/` when present. At least one of the two is `Some`
/// for every entry — a bare `.hallouminate/` with neither is not a wiki.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredWiki {
    pub repo_root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub wiki_dir: Option<PathBuf>,
}

/// Walk downward from `root` (no deeper than `max_depth`) and return every
/// sub-repo `.hallouminate` root, deduped by `repo_root`.
///
/// `root` itself is never reported: discovery is for repos *below* the
/// parent directory, and a request issued from inside a repo takes the
/// existing single-repo resolution path instead. The walk skips the contents
/// of `.git` directories unconditionally and applies `ignore`'s standard
/// filters per `ignore`.
pub fn discover_wiki_roots(
    root: &Path,
    max_depth: usize,
    ignore: &IgnoreRules,
) -> Vec<DiscoveredWiki> {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(ignore.respect_gitignore)
        .git_ignore(ignore.respect_gitignore)
        .git_global(ignore.respect_gitignore)
        .git_exclude(ignore.respect_gitignore)
        .ignore(ignore.respect_gitignore)
        .parents(false)
        // The built-in `hidden` filter prunes *all* dot directories, which
        // would also prune `.hallouminate` — the very thing being discovered.
        // Leave it off and apply the "skip hidden except .hallouminate" rule
        // via `filter_entry` below, so `.hallouminate` survives the prune.
        .hidden(false)
        .follow_links(false)
        // `max_depth` here bounds how deep entries are *yielded*. The walk
        // looks for `<dir>/.hallouminate`, so allow one extra level to reach
        // the `.hallouminate` directory entry sitting under a max-depth repo.
        .max_depth(Some(max_depth + 1));
    // Install a `filter_entry` unconditionally to prune `.git` directories
    // regardless of `skip_hidden`. The walk doc promises ".git is skipped
    // unconditionally"; gating this on `skip_hidden` would violate that
    // guarantee when the caller passes `skip_hidden: false`. Other hidden
    // dot directories are pruned only when `skip_hidden` is true.
    let root_path = root.to_path_buf();
    let skip_hidden = ignore.skip_hidden;
    builder.filter_entry(move |entry| {
        // Always keep the walk root (even if its own name starts with a dot).
        if entry.path() == root_path {
            return true;
        }
        let name = entry.file_name();
        // Always keep `.hallouminate` — it is the discovery target.
        if name == ".hallouminate" {
            return true;
        }
        // Always prune `.git`, regardless of skip_hidden.
        if name == ".git" {
            return false;
        }
        // Prune all other dot directories only when skip_hidden is set.
        if skip_hidden {
            return !name.to_str().map(|n| n.starts_with('.')).unwrap_or(false);
        }
        true
    });

    let mut out: Vec<DiscoveredWiki> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    target: "hallouminate::discovery",
                    error = %err,
                    "discovery walk: skipping unreadable entry"
                );
                continue;
            }
        };
        if entry.file_name() != ".hallouminate" {
            continue;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let hallou_dir = entry.path();
        let Some(repo_root) = hallou_dir.parent() else {
            continue;
        };
        // Never report the walk root itself; a request from inside a repo
        // uses the single-repo resolution path, not discovery.
        if repo_root == root {
            continue;
        }
        let config = hallou_dir.join("config.toml");
        let config_path = config.is_file().then_some(config);
        let wiki = repo_root.join(WIKI_RELATIVE_PATH);
        let wiki_dir = wiki.is_dir().then_some(wiki);
        // A `.hallouminate/` with neither a config nor a wiki dir is not a
        // discoverable wiki — skip it rather than fabricate an empty corpus.
        if config_path.is_none() && wiki_dir.is_none() {
            continue;
        }
        let repo_root = repo_root.to_path_buf();
        if seen.insert(repo_root.clone()) {
            out.push(DiscoveredWiki {
                repo_root,
                config_path,
                wiki_dir,
            });
        }
    }
    tracing::debug!(
        target: "hallouminate::discovery",
        roots = out.len(),
        root = %root.display(),
        "discovered sub-repo wikis"
    );
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Create `<root>/<repo>/.hallouminate/config.toml` and an empty
    /// `.hallouminate/wiki/` so the directory reads as a discoverable wiki.
    fn seed_repo_wiki(root: &Path, repo: &str) -> PathBuf {
        let repo_root = root.join(repo);
        let hallou = repo_root.join(".hallouminate");
        fs::create_dir_all(hallou.join("wiki")).expect("mkdir wiki");
        fs::write(hallou.join("config.toml"), "").expect("write config");
        repo_root
    }

    fn repo_roots(found: &[DiscoveredWiki]) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = found.iter().map(|w| w.repo_root.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn discovers_multiple_sibling_repo_wikis_below_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let alpha = seed_repo_wiki(tmp.path(), "alpha");
        let beta = seed_repo_wiki(tmp.path(), "beta");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());

        assert_eq!(
            repo_roots(&found),
            {
                let mut want = vec![alpha.clone(), beta.clone()];
                want.sort();
                want
            },
            "both sibling repo wikis must be discovered below the parent dir"
        );
        // Each carries its config + wiki dir attribution.
        let alpha_hit = found
            .iter()
            .find(|w| w.repo_root == alpha)
            .expect("alpha present");
        assert_eq!(
            alpha_hit.config_path,
            Some(alpha.join(".hallouminate").join("config.toml")),
            "config.toml path must be attributed to the discovered repo"
        );
        assert_eq!(
            alpha_hit.wiki_dir,
            Some(alpha.join(".hallouminate").join("wiki")),
            "wiki dir must be attributed to the discovered repo"
        );
    }

    #[test]
    fn does_not_report_the_walk_root_itself() {
        // A `.hallouminate` directly under the walk root belongs to a request
        // issued *from inside* a repo — that takes the single-repo path, not
        // discovery. Discovery must skip it so it isn't double-counted.
        let tmp = tempfile::tempdir().expect("tempdir");
        let hallou = tmp.path().join(".hallouminate");
        fs::create_dir_all(hallou.join("wiki")).expect("mkdir");
        fs::write(hallou.join("config.toml"), "").expect("write");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());
        assert!(
            found.is_empty(),
            "the walk root's own .hallouminate must not be reported: {found:?}"
        );
    }

    #[test]
    fn respects_gitignore_pruning_ignored_subtrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // The parent dir is itself a git repo with a `.gitignore` that hides
        // `vendor/`. A repo wiki buried in `vendor/` must not be discovered
        // when gitignore is respected.
        fs::create_dir_all(tmp.path().join(".git")).expect("mkdir .git");
        fs::write(tmp.path().join(".gitignore"), "vendor/\n").expect("write gitignore");
        let visible = seed_repo_wiki(tmp.path(), "visible");
        seed_repo_wiki(tmp.path(), "vendor");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());
        assert_eq!(
            repo_roots(&found),
            vec![visible],
            "gitignored vendor/ subtree must be pruned from discovery"
        );
    }

    #[test]
    fn finds_gitignored_wiki_when_gitignore_disabled() {
        // The mirror of the test above: with `respect_gitignore = false`, the
        // gitignored subtree IS walked. Locks the dichotomy so a regression
        // that ignores the flag is caught.
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join(".git")).expect("mkdir .git");
        fs::write(tmp.path().join(".gitignore"), "vendor/\n").expect("write gitignore");
        let visible = seed_repo_wiki(tmp.path(), "visible");
        let vendored = seed_repo_wiki(tmp.path(), "vendor");

        let rules = IgnoreRules {
            respect_gitignore: false,
            skip_hidden: true,
        };
        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &rules);
        assert_eq!(
            repo_roots(&found),
            {
                let mut want = vec![visible, vendored];
                want.sort();
                want
            },
            "with gitignore disabled, the vendored wiki must be discovered too"
        );
    }

    #[test]
    fn skips_hidden_directories_except_dot_hallouminate() {
        // A repo wiki nested inside a hidden `.cache/` dir must not be
        // discovered when hidden dirs are skipped — but the `.hallouminate`
        // dir of a normal sibling repo is still traversed (it's a dot dir,
        // yet it's the one exception).
        let tmp = tempfile::tempdir().expect("tempdir");
        let normal = seed_repo_wiki(tmp.path(), "normal");
        seed_repo_wiki(&tmp.path().join(".cache"), "buried");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());
        assert_eq!(
            repo_roots(&found),
            vec![normal],
            "a wiki under a hidden .cache/ dir must be skipped; the normal repo's \
             .hallouminate must still be found"
        );
    }

    #[test]
    fn honors_the_depth_cap() {
        // A repo wiki sitting deeper than the cap must not be discovered.
        let tmp = tempfile::tempdir().expect("tempdir");
        let shallow = seed_repo_wiki(tmp.path(), "shallow");
        // a/b/c/d/deep — five levels down, beyond a cap of 2.
        let deep_parent = tmp.path().join("a").join("b").join("c").join("d");
        let deep = seed_repo_wiki(&deep_parent, "deep");

        let found = discover_wiki_roots(tmp.path(), 2, &IgnoreRules::default());
        let roots = repo_roots(&found);
        assert!(
            roots.contains(&shallow),
            "shallow repo within the cap must be found: {roots:?}"
        );
        assert!(
            !roots.contains(&deep),
            "repo beyond the depth cap must not be found: {roots:?}"
        );
    }

    #[test]
    fn skips_bare_hallouminate_with_no_config_or_wiki() {
        // A `.hallouminate` dir that holds neither a config nor a wiki/ is not
        // a discoverable wiki; fabricating an empty corpus for it would be a
        // phantom result.
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("bare");
        fs::create_dir_all(bare.join(".hallouminate")).expect("mkdir bare .hallouminate");
        let real = seed_repo_wiki(tmp.path(), "real");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());
        assert_eq!(
            repo_roots(&found),
            vec![real],
            "bare .hallouminate with no config/wiki must be skipped"
        );
    }

    #[test]
    fn reports_wiki_only_repo_when_config_absent() {
        // A repo with a `.hallouminate/wiki/` but no config.toml is still a
        // discoverable wiki (config_path None, wiki_dir Some).
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("wikionly");
        fs::create_dir_all(repo_root.join(".hallouminate").join("wiki")).expect("mkdir");

        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default());
        assert_eq!(
            found.len(),
            1,
            "wiki-only repo must be discovered: {found:?}"
        );
        assert_eq!(found[0].config_path, None);
        assert_eq!(
            found[0].wiki_dir,
            Some(repo_root.join(".hallouminate").join("wiki"))
        );
    }

    #[test]
    fn prunes_git_dirs_unconditionally_even_when_skip_hidden_false() {
        // Fix 4: the `.git` prune was gated on `skip_hidden`, so a walk with
        // `skip_hidden: false` would descend into `.git` subtrees. The filter is
        // now unconditional: `.git` is always pruned; only OTHER hidden dirs are
        // conditional.
        let tmp = tempfile::tempdir().expect("tempdir");

        // A wiki buried under `.git/` — must NEVER be discovered.
        let git_buried = tmp.path().join(".git").join("buried");
        let hallou_git = git_buried.join(".hallouminate");
        fs::create_dir_all(hallou_git.join("wiki")).expect("mkdir .git buried wiki");
        fs::write(hallou_git.join("config.toml"), "").expect("write .git buried config");

        // A wiki buried under `.cache/` — must be discovered when skip_hidden=false.
        let cache_buried = tmp.path().join(".cache").join("cached");
        let hallou_cache = cache_buried.join(".hallouminate");
        fs::create_dir_all(hallou_cache.join("wiki")).expect("mkdir .cache buried wiki");
        fs::write(hallou_cache.join("config.toml"), "").expect("write .cache buried config");

        let rules = IgnoreRules {
            respect_gitignore: false,
            skip_hidden: false,
        };
        let found = discover_wiki_roots(tmp.path(), DEFAULT_MAX_DEPTH, &rules);
        let roots: std::collections::HashSet<std::path::PathBuf> =
            found.iter().map(|w| w.repo_root.clone()).collect();

        assert!(
            !roots.contains(&git_buried),
            ".git-buried wiki must not be discovered (unconditional .git prune): {roots:?}"
        );
        assert!(
            roots.contains(&cache_buried),
            ".cache-buried wiki must be discovered when skip_hidden=false: {roots:?}"
        );
    }
}
