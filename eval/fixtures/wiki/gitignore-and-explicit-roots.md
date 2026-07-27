---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - crates/hallouminate-domain/src/corpus/walker.rs
  - https://github.com/paulnsorensen/hallouminate/issues/232
---
# Gitignore-aware walking and the explicit-root escape hatch

Corpus scanning honours `.gitignore` by default, but a configured root that is itself gitignored flips that off entirely for that root. The rule exists so a directory the user pointed at on purpose — even one their own `.gitignore` excludes — still gets indexed.

## Default: honour git's own ignore rules

`scan` builds each root's walk with `ignore::WalkBuilder` and `standard_filters(true)`, which pulls in `.gitignore`, `.ignore`, `.git/info/exclude`, and the user's global gitignore, walking up from the root to the nearest `.git` boundary.[^1] Dotfiles are not special-cased — they are content like anything else, and are only skipped when gitignore rules say so, not because they start with a dot.[^2] This default is deliberate: a corpus root usually sits inside a git checkout, and a user who has told git to ignore `target/`, `node_modules/`, or a build artifact directory almost certainly does not want that content pulled into their wiki search index either.

## The surprise this default would otherwise cause

Without an escape hatch, the gitignore default has one bad failure mode: a user whose notes live in a directory their own `.gitignore` excludes — a personal `notes/` folder, a `scratch/` directory, anything covered by a broad `*.md` exclude rule written for an unrelated reason — configures that directory as a corpus root and gets an empty index back, with no files scanned and no obvious explanation why. The directory exists, the files exist, the config points at the right place, and nothing shows up. That is exactly the kind of silent, hard-to-diagnose gap this project avoids elsewhere (compare the corpus write sandbox's preference for loud, specific errors over swallowed failures).

## The opt-in: a gitignored root is explicit intent

`scan` checks whether each configured root itself is gitignored before walking it, via `root_is_gitignored`: walk upward from the root collecting every `.gitignore` along the way until a `.git` boundary is found, then ask the `ignore` crate's own `Gitignore` matcher whether it would consider the root itself ignored.[^3] When that's true, `walk_root` disables every ignore mechanism for that root — `git_ignore`, `git_global`, `git_exclude`, `ignore`, and `parents` are all turned off — so the walk proceeds as if no gitignore existed for it at all.[^4] The reasoning is spelled out at the call site: "if the corpus root itself is gitignored by some ancestor `.gitignore`, the user pointed at it on purpose — treat that as explicit opt-in and walk it without applying gitignore filters."[^5] Naming a gitignored path as a corpus root *is* the opt-in; there is no separate config flag to set.

A subtlety worth calling out: this opt-in is root-scoped, not global. If a gitignored root contains a nested subdirectory with its own more specific `.gitignore` exclusions, those still apply to files *within* the walk — only the fact that the root itself is ignored gets bypassed, not gitignore checking wholesale for everything under it, because `explicit_opt_in` only disables the mechanisms `WalkBuilder` would otherwise use to skip the root's own contents.

## Failure modes default to honouring gitignore

`root_is_gitignored` returns `false` — meaning gitignore filtering stays *on* — on every structural surprise: no `.git` boundary found walking upward, or the collected `.gitignore` files fail to build into a matcher.[^6] A single malformed glob line in an ancestor `.gitignore` (including the user's own global gitignore) does not disable the check either; the `ignore` crate's partial-error behaviour for one bad line still lets every other valid line in that file take effect, so the code deliberately swallows that specific non-fatal error rather than treating it as fatal and silently losing the whole opt-in path.[^7] The asymmetry is intentional: the default is to respect gitignore, and the code only overrides that default when it can prove — not guess — that the user explicitly named a path git itself would otherwise skip.

See [worktree-corpus-identity](worktree-corpus-identity.md), [sandbox-and-workspace-roots](sandbox-and-workspace-roots.md), [architecture](architecture.md).

[^1]: `crates/hallouminate-domain/src/corpus/walker.rs:123-128`
[^2]: `crates/hallouminate-domain/src/corpus/walker.rs:126-127`
[^3]: `crates/hallouminate-domain/src/corpus/walker.rs:180-198`
[^4]: `crates/hallouminate-domain/src/corpus/walker.rs:129-136`
[^5]: `crates/hallouminate-domain/src/corpus/walker.rs:76-82`
[^6]: `crates/hallouminate-domain/src/corpus/walker.rs:195-213`
[^7]: `crates/hallouminate-domain/src/corpus/walker.rs:201-210`

_Source: crates/hallouminate-domain/src/corpus/walker.rs · Updated: 2026-07-18_
