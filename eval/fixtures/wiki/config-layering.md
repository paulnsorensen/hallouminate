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
- overrides scalar config (`top_files_default`, `chunks_per_file_default`, embeddings model, etc.)

Since commit `bf14888` (auto-discovery), the repo layer is **required**
— a CLI invocation from inside a directory without an ancestor
`.hallouminate/config.toml` errors out with `hallouminate requires a
.hallouminate/config.toml in the working directory's repo`. An empty
file is enough to satisfy the check.

### Relative paths resolve against the repo root

`load_repo_layer` (`src/app/config.rs::load_repo_layer` →
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

## Merge semantics

Implemented in `src/app/config.rs::merge_layers`. Rules:

- Arrays (`[[corpus]]`, `[[repository]]`) — repo entries are appended after baseline entries. Duplicate names error.
- Scalars (`top_files_default`, etc.) — repo wins if it sets a non-default value; otherwise baseline. Conflicting non-default values error with both source paths named.

Conflict messages always name the source path of the offending value:

```text
config: scalar conflict on `top_files_default`:
  /Users/paul/.config/hallouminate/config.toml says 10
  /Users/paul/Dev/hallouminate/.hallouminate/config.toml says 5
```

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
