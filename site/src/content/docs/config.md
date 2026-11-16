---
title: "Configuration"
---

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
| `[logging]` | Process-wide log rotation and retention limits. |
| `[watch]` | File-watch settings. |
| `[storage]` | Ground-directory location. |

## The XDG baseline vs the repo layer

The **baseline** owns explicit `[[corpus]]` entries, `[[repository]]`
declarations, and the `[search]`/`[embeddings]`/`[logging]`/`[watch]`/`[storage]`
defaults. It is loaded once at daemon startup — change it and restart the
daemon. `[logging]` is process-wide and baseline-owned — a repo layer that
sets a non-default `[logging]` errors, naming the repo config path.

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
  non-default values error and name both source paths. `[logging]` stays
  baseline-owned: a repo layer that sets a non-default `[logging]` is an
  ERROR naming the repo config path, rather than being silently ignored.

## Logging

The active process log is
`$XDG_STATE_HOME/hallouminate/hallouminate.log` (default
`~/.local/state/hallouminate/hallouminate.log`). Size rotation creates
numbered archives `hallouminate.log.1`, `.2`, and so on, with `.1` newest.

```toml
[logging]
max_file_bytes = 10485760   # 10 MiB
max_total_bytes = 104857600 # 100 MiB
```

Every active or archived file is capped at `max_file_bytes`. Retention keeps at
most `floor(max_total_bytes / max_file_bytes)` files including the active file,
so defaults retain the active file plus nine archives and never exceed 100 MiB.
Both limits must be nonzero and the total must be at least the per-file limit.

`HALLOUMINATE_LOG_MAX_FILE_BYTES` and
`HALLOUMINATE_LOG_MAX_TOTAL_BYTES` override the baseline limits. Invalid values
stop startup and name the offending variable. Log filtering remains
`HALLOUMINATE_LOG`, then `RUST_LOG`, then `hallouminate=info`.

### Operations log

Each operation appends one JSON row to a daily segment file
`$XDG_STATE_HOME/hallouminate/operations-YYYY-MM-DD.jsonl`, named by UTC date.
Read the log with the glob `operations-*.jsonl`.

```toml
[logging]
operations = true                    # set false to write no rows
operations_max_total_bytes = 52428800 # 50 MiB across all segments
```

The log is on by default. When the segments together pass
`operations_max_total_bytes`, the daemon deletes the oldest segments after its
own append. It never deletes the segment for the current UTC date, and other
processes never delete segments. Each process reads both keys once at its
start. Both keys are baseline-owned like the rest of `[logging]`, so a
repository configuration that sets them to a non-default value is an error.

See [Operations log](/hallouminate/operations-log/) for the row schema.

## Watcher failure reminders

`[watch].failure_reminder_secs` defaults to `60`. Identical watcher reindex
failures (same path and rendered error) log once, then the next occurrence at
or after the interval reports how many messages were suppressed. Distinct
failures remain immediate. Set the value to `0` to disable suppression, or use
`HALLOUMINATE_WATCH_FAILURE_REMINDER_SECS` as a process override.

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
| Active process log | `$XDG_STATE_HOME/hallouminate/hallouminate.log` (state-dir fallback) |
| Rotated process logs | `hallouminate.log.1` through `.9` with default limits |
| Ground (LanceDB) directory | `~/.local/share/hallouminate/ground` |
| Model cache | `~/.cache/hallouminate/fastembed` |
| Daemon socket | `<account-home>/.cache/hallouminate/daemon.sock` |
