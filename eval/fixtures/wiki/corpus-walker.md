# Corpus walker

The walker turns a `CorpusConfig` (paths + include globs + exclude
globs) into a list of `(FileRef, Mtime)` pairs. Lives at
`src/domain/corpus/walker.rs`. As of commit `f5c5224` (with a follow-up
fix in `39c3908`), it's built on BurntSushi's `ignore` crate — the
same gitignore-aware walker ripgrep uses — instead of `walkdir`.

## Gitignore-aware by default

The walker honors `.gitignore`, `.ignore`, `.git/info/exclude`, and the
user's global gitignore. Hidden files (dotfiles) are walked — only
gitignore decides what's filtered. Concretely:

- Anything in this repo's `.gitignore` (`/target`, `.cheese/`, `ralphs/`, `.code-review-graph/`) is skipped automatically.
- A user-level global gitignore (e.g. `~/.config/git/ignore` listing `.DS_Store`) applies too.
- `.git/` directories are pruned by the `ignore` crate's standard filters.

## Explicit-root opt-in

If the corpus root is itself gitignored (per ancestor `.gitignore`
files), the walker treats that as the user pointing at gitignored
content on purpose and walks it with gitignore disabled for the whole
walk. No flag needed — the geometry of the corpus path is the signal.

Example from this repo's config:

```toml
[[corpus]]
name = "cheese-local"
paths = ["~/Dev/hallouminate/.cheese"]
globs = ["**/*.md"]
```

`.cheese/` is in the repo's `.gitignore`. Without the opt-in, the
walker would refuse to descend. With it, `root_is_gitignored(...)`
returns true, and the walk proceeds normally.

## Implementation sketch

`root_is_gitignored(root: &Path) -> bool`:

1. Walk up from `root.parent()` looking for a `.git` boundary; collect every `.gitignore` along the way.
2. Build an `ignore::gitignore::Gitignore` from those files (outer-to-inner so inner overrides outer).
3. Ask `matched_path_or_any_parents(root, root.is_dir()).is_ignore()`.

On any structural surprise (no `.git` ancestor, gitignore build error)
the helper returns `false`. The conservative default is to honor
gitignore rather than to silently bypass it.

`GitignoreBuilder::add` returns `Some(_)` for non-fatal partial errors
(a single malformed glob line). Per the `ignore` crate docs, every
other valid glob in the file is still added. The walker drops that
partial error rather than bailing — otherwise a stray bad line in any
ancestor `.gitignore` (including the user's global) would silently
disengage the opt-in detection.

Regression guards:

- `root_is_gitignored_distinguishes_opt_in_from_default_paths` — both branches behave differently for paths that are vs are not gitignored.
- `root_is_gitignored_returns_false_when_no_git_ancestor` — no `.git` ancestor yields false.
- `root_is_gitignored_survives_malformed_ancestor_gitignore` — a malformed line above a valid `secrets/` rule still detects `secrets/` as opt-in.

## What this means for `corpus_exclude`

After the gitignore-aware change, `corpus_exclude` only needs to list
paths that are NOT in `.gitignore`. For this repo:

```toml
corpus_exclude = [
  "**/.dogfood/**",       # not gitignored
  "**/.hallouminate/**",  # not gitignored — keeps the wiki out of the source corpus
  "**/.claude/**",        # not gitignored — Claude Code worktree shadow copies
]
# target/, .cheese/, ralphs/, .code-review-graph/ are filtered by .gitignore.
```
