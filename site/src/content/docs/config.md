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

Each process writes its own log file in `$XDG_STATE_HOME/hallouminate/`
(default `~/.local/state/hallouminate/`). Only one process writes each file:

| Process | Log file |
|---|---|
| Daemon | `daemon.log` (`daemon-<pid>.log` if another live daemon holds `daemon.log`) |
| MCP stdio server (`hallouminate serve`) | `serve-<pid>.log` |
| Any other CLI command | `cli-<pid>.log`, created only when the command logs |

A file rotates before a record would take it past `max_file_bytes`. Archives
are `<name>.log.1`, `.2`, and so on, with the highest number newest.

```toml
[logging]
max_file_bytes = 10485760   # 10 MiB
max_total_bytes = 104857600 # 100 MiB
```

`max_total_bytes` bounds all of these files together, plus legacy
`hallouminate.log` and `hallouminate.log.N` files from older versions. At
startup and after each rotation, a process removes oversized archives it owns
or can claim. It then deletes eligible files oldest-first until the directory
has room for one more full file. It does not delete its active file or files
of another live process. If a prior active file exceeds a lowered file limit,
it rotates before the next valid record and its oversized archive is removed.
Each writer holds an advisory lock on `<name>.log.lock` while it runs. A stable
per-directory lock serializes retention passes across processes. Live files of
other processes can keep the total above the limit until those processes exit.
Records larger than `max_file_bytes` are dropped without stderr payload output.
Both limits must be nonzero and the total must be at least the per-file limit.
New process logs and bootstrap captures are owner-only on Unix. Existing file
permissions are not changed. Stored diagnostics can contain sensitive panic
payloads.

A panic in any thread is logged as an `ERROR` event with target
`hallouminate::panic`. The event carries the thread name, location, payload,
and a backtrace. The panic message also still goes to stderr.

`HALLOUMINATE_LOG_MAX_FILE_BYTES` and
`HALLOUMINATE_LOG_MAX_TOTAL_BYTES` override the baseline limits. Invalid values
stop startup and name the offending variable. Log filtering remains
`HALLOUMINATE_LOG`, then `RUST_LOG`, then `hallouminate=info`.

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
| Process logs | `$XDG_STATE_HOME/hallouminate/` (state-dir fallback): `daemon.log`, `serve-<pid>.log`, `cli-<pid>.log` |
| Rotated process logs | `<name>.log.1`, `.2`, and so on, highest newest |
| Ground (LanceDB) directory | `~/.local/share/hallouminate/ground` |
| Model cache | `~/.cache/hallouminate/fastembed` |
| Daemon socket | `<account-home>/.cache/hallouminate/daemon.sock` |
| Daemon bootstrap log | `$XDG_STATE_HOME/hallouminate/daemon-bootstrap.log` |

These are the defaults for a release build. A dev build uses other
defaults, as the next section shows.

## Release builds and dev builds

A release build and a dev build never share a default store or daemon.
This stops a dev build from migrating the store of an installed release.

A build is a release build when one of these conditions is true at compile
time:

- The build sets `HALLOUMINATE_RELEASE_CHANNEL` to a non-empty value. The
  nightly workflow (`@paulnsorensen/hallouminate-nightly` on npm) sets it to
  `nightly`. The tag release workflow (shell installer and the
  `hallouminate` npm package) sets it to `stable`.
- The source comes from a published crates.io package, for example
  `cargo install hallouminate`. The `hallouminate-config` build script
  detects the `.cargo_vcs_info.json` file that `cargo package` writes.

All other builds are dev builds. Examples are `cargo build` or `cargo test`
in a checkout or worktree, and `cargo install --path`. A release profile
does not make a build a release build.

Packagers: `cargo install --git`, `cargo install --path`, and distro or Nix
builds from a git tarball are dev builds. Set `HALLOUMINATE_RELEASE_CHANNEL`
at build time to make them release builds.

`hallouminate --version` prints the channel after the version, for example
`hallouminate 0.12.2 (stable)`. A dev build prints `(dev)`.

A dev build replaces the `hallouminate` directory name with
`hallouminate-dev` in these defaults:

| What | Release build | Dev build |
|---|---|---|
| Ground (LanceDB) directory | `~/.local/share/hallouminate/ground` | `~/.local/share/hallouminate-dev/ground` |
| Daemon socket | `<account-home>/.cache/hallouminate/daemon.sock` | `<account-home>/.cache/hallouminate-dev/daemon.sock` |
| Daemon bootstrap log | `$XDG_STATE_HOME/hallouminate/daemon-bootstrap.log` | `$XDG_STATE_HOME/hallouminate-dev/daemon-bootstrap.log` |

The watchdog trip file is next to the socket, so it also moves.

These items do not change:

- Both builds read the same baseline config,
  `$XDG_CONFIG_HOME/hallouminate/config.toml`.
- Both builds share the model cache, `~/.cache/hallouminate/fastembed`.
  Model files are immutable downloads and have no store schema.
- Explicit settings always win. These are `[storage].ground_dir`,
  `[embeddings].cache_dir`, and `HALLOUMINATE_SOCKET`. A baseline that sets
  `ground_dir` gives both builds the same store.
- `XDG_STATE_HOME` still sets the base of the state directory. A dev build
  adds `hallouminate-dev` below it.
- The active process log stays at
  `$XDG_STATE_HOME/hallouminate/hallouminate.log` for both builds.

The first dev build on a host starts with an empty store. Run
`hallouminate index` to fill it.

## Daemon socket path length

A Unix socket path must fit in `sockaddr_un.sun_path`, including the
trailing NUL. The limit is 108 bytes on Linux and 104 bytes on macOS. The
daemon and its clients check the resolved socket path before they use it.
A path that is too long stops the command with an error that names
`HALLOUMINATE_SOCKET` and the limit.
