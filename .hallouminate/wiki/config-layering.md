# Config layering

A daemon serves many repos with different configurations. Two layers
combine per request: an XDG baseline loaded once at daemon boot, and a
repo-layer config discovered fresh on every RPC from the client's
`cwd`.

## Baseline (XDG)

Path: `${XDG_CONFIG_HOME:-~/.config}/hallouminate/config.toml`.

Loaded once at `hallouminate daemon` startup. Owns:

- explicit `[[corpus]]` entries (e.g. `cheese-global`, `cheese-local`)
- `[[repository]]` declarations (each derives a `repo:NAME:wiki` and a `repo:NAME:corpus`)
- defaults: `[search]`, `[embeddings]`, `[watch]`, `[storage]`

To change the baseline, restart the daemon:

```sh
pkill -f 'hallouminate daemon'
hallouminate daemon &
```

## Repo layer

Path: `<repo>/.hallouminate/config.toml`, discovered by walking up from
the client's `cwd` until a `.git` boundary or the filesystem root.

The repo layer overrides or augments the baseline:

- adds repo-local corpora (typically empty in practice — most repos let the baseline's `[[repository]]` entry derive their corpora)
- overrides scalar config (`top_files_default`, `chunks_per_file_default`, embeddings model, etc.), except the baseline-only fields listed under [Merge semantics](#merge-semantics)

Since commit `bf14888` (auto-discovery), the repo layer is **required**
— a CLI invocation from inside a directory without an ancestor
`.hallouminate/config.toml` errors out with `hallouminate requires a
.hallouminate/config.toml in the working directory's repo`. An empty
file is enough to satisfy the check.

### Relative paths resolve against the repo root

`load_repo_layer` (`crates/hallouminate-config/src/lib.rs::load_repo_layer` →
`resolve_repo_path`) rewrites relative paths in the repo layer against
the **repo root** — the parent of `.hallouminate/`, not `.hallouminate/`
itself. Absolute and `~`-prefixed paths pass through untouched. So a
repo-layer `[[repository]] path = "."` resolves to the repo root, and
`wiki_directory` lands it at `<repo>/.hallouminate/wiki`.

This repo self-declares with exactly that — `.hallouminate/config.toml`
holds:

```toml
[[repository]]
name = "hallouminate"
path = "."
```

so its own wiki is searchable as `repo:hallouminate:wiki` from any
checkout or worktree with no per-machine baseline entry. This is the
exception to "repo corpora are typically empty" above. Don't also
declare `hallouminate` in the XDG baseline: both layers would derive
`repo:hallouminate:wiki` and collide on the duplicate-name check.

A wiki corpus is **not** auto-discovered just because a
`.hallouminate/wiki/` directory exists under `cwd` — the corpus only
exists once a `[[repository]]` (baseline or repo layer) declares it.


### Optional global corpus inheritance

Set this top-level key in `<repo>/.hallouminate/config.toml` to exclude
inherited global corpus and repository declarations:

```toml
inherit_global_corpora = false

[[repository]]
name = "myrepo"
path = "."
```

Omitting the key or setting it to `true` preserves inheritance.
The repository layer controls this policy; the baseline value does not
control repository requests. Global search, embedding, storage, watch,
logging, and daemon settings retain their existing merge rules.[^corpus-policy]

This policy changes the effective corpus set, not only search results.
It does not delete globally indexed documents. A corpus-less `ground`
request searches the remaining effective corpora.[^corpus-policy-ground]

Global `[[corpus]]` entries need neither a wiki nor a `global = true`
marker. A `[[repository]]` entry still derives its repository wiki.[^optional-global-wiki]

[^corpus-policy]: crates/hallouminate-config/src/lib.rs::Config; crates/hallouminate-config/src/lib.rs::merge_layers_with_sources
[^corpus-policy-ground]: crates/hallouminate-daemon/src/dispatch.rs::handle_ground
[^optional-global-wiki]: crates/hallouminate-config/src/lib.rs::validate; crates/hallouminate-domain/src/repository.rs::effective_corpora

## Merge semantics

Implemented in `crates/hallouminate-config/src/lib.rs::merge_layers`. Rules:

- Arrays (`[[corpus]]`, `[[repository]]`) append repository entries after baseline entries by default. Duplicate names error.
- Repository `inherit_global_corpora = false` excludes both baseline declaration arrays before the merge. Local declarations remain.[^corpus-policy]
- Scalars (`top_files_default`, etc.) — repo wins if it sets a non-default value; otherwise baseline. Conflicting non-default values error with both source paths named.
- Baseline-only scalars — `storage.ground_dir`, `embeddings.cache_dir`, `watch.reconcile_interval_secs`, and `watch.catch_up_concurrency` (`merge_baseline_only`). The active value is the baseline value, or the built-in default when the baseline omits the key. A repo layer may omit the key or restate the active value. For `storage.ground_dir` and `embeddings.cache_dir` only, a repo value equal to the built-in default counts as omitted, so a baseline value stays active. Any other repo value is an error, even when the baseline file does not set the key.[^baseline-only] `[logging]` and `[security]` are also baseline-owned; any non-default repo block errors.
- `inherit_global_corpora = false` does not relax these rules. A repo cannot get a private `ground_dir`; all repos share one store, and its rows are keyed by root.

Conflict messages always name the source path of the offending value:

```text
config: scalar conflict on search.top_files_default: baseline = 10 (baseline at
  /Users/paul/.config/hallouminate/config.toml), repo = 5 (repo at
  /Users/paul/Dev/hallouminate/.hallouminate/config.toml)
```

A baseline-only conflict says the field is baseline-only, tells whether the active value is the built-in default, and says how to fix it:

```text
config: scalar conflict on storage.ground_dir: storage.ground_dir is
  baseline-only, so a repo config cannot change it; active =
  "~/.local/share/hallouminate/ground" (built-in default; not overridden by
  baseline at ~/.config/hallouminate/config.toml), repo =
  ".hallouminate/ground" (repo at <repo>/.hallouminate/config.toml). Remove
  storage.ground_dir from the repo config, or set it in the baseline config.
```

`hallouminate config show` and `config validate` print `repo: <path> (found, failed to load)` for a repo config that exists but fails to load or merge. They print `repo: not found` only when discovery finds no repo config.

[^baseline-only]: crates/hallouminate-config/src/lib.rs::merge_baseline_only; made baseline-only for `ground_dir` and `cache_dir` in #612 (commit `c1e38c4`).

## Per-request flow

Every daemon RPC carries the client's `cwd`. The daemon:

1. Walks up from `cwd` to find `.hallouminate/config.toml`.
2. Loads the repo layer.
3. Merges with the cached baseline.
4. Dispatches the request against the merged config.

The baseline is loaded once and cached; the repo layer is read from
disk on every request. That's deliberate — repo-layer edits take
effect on the next RPC without needing a daemon restart, but baseline
edits do require restart.

## Inspecting

```sh
hallouminate config show           # effective merged config from the current cwd
hallouminate config validate       # parse + flag unknown top-level keys
hallouminate config init           # scaffold a baseline at XDG_CONFIG_HOME
```
