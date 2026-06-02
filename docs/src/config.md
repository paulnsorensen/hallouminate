# Configuration

Config lives at `$XDG_CONFIG_HOME/hallouminate/config.toml`
(`~/.config/hallouminate/config.toml` by default). Two layers combine per
request: an **XDG baseline** loaded once at daemon boot, and a **repo layer**
discovered fresh on every request from the client's working directory.

```sh
hallouminate config init       # scaffold the baseline
hallouminate config show       # the effective merged config for this cwd
hallouminate config validate   # parse + flag unknown top-level keys
```

## Sections

| Section | Holds |
|---|---|
| `[[corpus]]` | Explicit named corpora (`name`, `paths`, `globs`, exclude rules). |
| `[[repository]]` | Repo declarations; each derives `repo:NAME:wiki` and `repo:NAME:corpus`. |
| `[search]` | Read-side defaults (`top_files_default`, `chunks_per_file_default`, …). |
| `[embeddings]` | Embedding model and toggle (below). |
| `[watch]` | File-watch settings. |
| `[storage]` | Ground-directory location. |

## The XDG baseline vs the repo layer

The **baseline** owns explicit `[[corpus]]` entries, `[[repository]]`
declarations, and the `[search]`/`[embeddings]`/`[watch]`/`[storage]`
defaults. It is loaded once at daemon startup — change it and restart the
daemon.

The **repo layer** is `<repo>/.hallouminate/config.toml`, found by walking up
from the cwd to the first `.git` boundary. It overrides scalars and adds
repo-local corpora, and is re-read on every request — so repo-layer edits take
effect without a daemon restart. The repo layer is **required**: a CLI
invocation from a directory with no ancestor `.hallouminate/config.toml`
errors out. An empty file satisfies the check.

A repo declares itself like this repo does:

```toml
[[repository]]
name = "hallouminate"
path = "."
```

`path = "."` resolves against the repo root (the parent of `.hallouminate/`),
so the wiki lands at `<repo>/.hallouminate/wiki` and is searchable as
`repo:hallouminate:wiki` from any checkout.

### Merge rules

- Array entries (`[[corpus]]`, `[[repository]]`) — repo entries append after
  baseline entries; duplicate names error.
- Scalars — the repo wins if it sets a non-default value; conflicting
  non-default values error and name both source paths.

## Embeddings

Dense embeddings are **on by default**, using
`snowflake/snowflake-arctic-embed-s`. On first index hallouminate downloads
that model and fuses its vector signal with lexical search.

### Supported models

All embed to 384-dim vectors. Omitting `embeddings.model` selects the default.

| Model | Notes |
|---|---|
| `snowflake/snowflake-arctic-embed-s` | **Default.** English, symmetric retrieval. |
| `BAAI/bge-small-en-v1.5` | English, symmetric retrieval. |
| `intfloat/multilingual-e5-small` | Multilingual, asymmetric retrieval; no quantized variant. |

### Turning embeddings off

Run lexically only — full-text search + ripgrep + rerank, no embedding model
downloaded (just the tokenizer used for chunking):

```toml
[embeddings]
enabled = false
```

Changing the embedding mode (or model) for a ground directory already indexed
under a different mode trips the store's mismatch guard on the next run.
Delete the ground directory and re-index to rebuild:

```sh
rm -rf ~/.local/share/hallouminate/ground
hallouminate index
```

To pre-fetch the model so the first index doesn't pay the download cost:

```sh
hallouminate config download
```

## Paths at a glance

| What | Default |
|---|---|
| Baseline config | `$XDG_CONFIG_HOME/hallouminate/config.toml` |
| Repo-layer config | `<repo>/.hallouminate/config.toml` |
| Ground (LanceDB) directory | `~/.local/share/hallouminate/ground` |
| Model cache | `~/.cache/hallouminate/fastembed` |
| Daemon socket | `$XDG_RUNTIME_DIR/hallouminate/daemon.sock` (cache-dir fallback) |
