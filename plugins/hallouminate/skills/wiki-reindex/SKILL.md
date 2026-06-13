---
name: wiki-reindex
description: Recover from a LanceDB schema version mismatch by stopping the daemon, deleting the stale ground store, restarting, and re-running the index. Use when hallouminate fails to start with a "store schema version mismatch" error — "/wiki-reindex", "schema version mismatch", "reindex after upgrade", "ground store out of date". Surfaces the engine's existing error message as an actionable, copy-pasteable repair sequence. Do NOT use to bootstrap a new wiki (use wiki-init) or to fold new documents into an existing corpus (use wiki-ingest).
---

# wiki-reindex — recover from a schema version mismatch

When the hallouminate engine detects that the on-disk LanceDB store was
written by a different schema version than the running binary, it refuses
to open the store and prints:

```
store schema version mismatch: store has schema_version <old>, this build
expects <new>; delete <store-path> and re-run `hallouminate index` to rebuild
```

This skill turns that message into a safe, four-step repair sequence.

**Engine source:** `src/adapters/lance.rs` — `meta_check_or_init` (lines 152-159).

## Prerequisites

Before deleting anything, confirm you are looking at a genuine schema mismatch
(not a permissions error or a missing config):

```bash
# The error surfaces at daemon startup — check the daemon's output or run:
hallouminate daemon status
```

If the output contains `store schema version mismatch`, proceed. If it shows
`running`, the store is healthy — stop here.

## Step 1 — Identify the store directory

The error message includes the store path. Extract it:

```
... delete <store-path> and re-run `hallouminate index` to rebuild
```

If you cannot locate the exact path from the error output, the default store
lives at:

```
~/.local/share/hallouminate/ground
```

This path is overridable via `[storage] ground_dir` in your
`~/.config/hallouminate/config.toml` (or `XDG_DATA_HOME`). Verify:

```bash
hallouminate config show | grep ground_dir
```

Confirm the path is correct before proceeding. Deleting the wrong directory
causes data loss — the ground store is rebuilt from your source wikis, but
verify you know where each corpus lives (`hallouminate config show`) before
running the next step.

## Step 2 — Stop the daemon

```bash
hallouminate daemon stop
```

This sends a graceful shutdown request over the control socket and waits for
the socket file to disappear (up to 10 s). If no daemon is running the command
is a no-op.

## Step 3 — Delete the stale store

Replace `<store-path>` with the path from Step 1:

```bash
rm -rf <store-path>
```

Typical invocation using the default path:

```bash
rm -rf ~/.local/share/hallouminate/ground
```

> This removes only the LanceDB vector store. Your source wikis
> (`.hallouminate/wiki/` under each repo) are untouched.

## Step 4 — Restart the daemon

```bash
hallouminate daemon restart
```

This stops any running daemon (if you skipped Step 2) and spawns a fresh one,
then waits until it is reachable.

## Step 5 — Re-run the index

**Index a single corpus:**

```bash
hallouminate index --corpus <corpus-name>
```

For a `[[repository]]`-derived wiki corpus the name takes the form
`repo:<name>:wiki` (e.g. `repo:tern:wiki`).

**Index every configured corpus:**

```bash
hallouminate index
```

Omitting `--corpus` indexes all corpora visible to the daemon (all `[[corpus]]`
and `[[repository]]`-derived entries in the resolved config).

Watch the JSON output — each corpus entry reports `files_upserted`,
`chunks_inserted`, and `embeddings_inserted`. A non-empty corpus with zero
counts for all three is a signal that the root path was not found; check
`warnings` in the output.

## Verify

```bash
hallouminate daemon status
```

Expected output: `running`. If it reports `not running`, check the daemon logs
or run `hallouminate daemon` (foreground) to see startup errors.

## Preventing future mismatches

Schema version bumps happen when the LanceDB store layout changes between
releases. They are intentional breaking changes — the engine refuses to open
a store it cannot safely read. After any `hallouminate` binary upgrade, run
this skill proactively if the daemon fails to start.

If you version-pin the binary (e.g. via a project lockfile), coordinate the
binary upgrade with a reindex so your team's daemons stay in sync.

## Rules

- Only delete the store when a schema version mismatch is confirmed. Do not
  run `rm -rf` speculatively.
- The store is derived data — rebuild time grows with corpus size. On very
  large corpora, plan for the index step to take several minutes.
- If `hallouminate daemon stop` times out (> 10 s), kill the process directly
  (`kill $(lsof -t <socket-path)`) and then delete the socket file before
  proceeding.
