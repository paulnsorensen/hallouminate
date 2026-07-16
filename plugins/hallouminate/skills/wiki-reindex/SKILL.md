---
name: wiki-reindex
description: Triage and repair a hallouminate ground-store mismatch at daemon startup. Use when hallouminate fails to start or complains about the store — "store schema stale", "store schema version ... is NEWER than this build expects", "embedding store mismatch", "/wiki-reindex", "reindex after upgrade", "ground store out of date". Do NOT use to bootstrap a new wiki (use wiki-init) or to fold new documents into an existing corpus (use wiki-ingest).
---

# wiki-reindex — recover a mismatched ground store

The engine validates the on-disk LanceDB store against the running binary at
daemon startup. Three distinct mismatches surface there — and only two need
your hands. Triage before touching anything.

**Engine source:** validation in `meta_check_or_init`
(`crates/hallouminate-adapters/src/lance.rs`); stale-store auto-heal in
`move_stale_store` and the startup rebuild
(`crates/hallouminate/src/daemon/state.rs`).

## Prerequisites — see the real error

The mismatch surfaces at daemon **startup** — not via `daemon status`, which
is a liveness probe that only ever prints `running` / `not running` and never
reads the store. Run the daemon in the foreground:

```bash
# Prints the startup error straight to your terminal (Ctrl-C to exit):
hallouminate daemon
```

If a corpus is auto-spawned (e.g. by `hallouminate serve`) the same startup
error is captured in the bootstrap log instead of your terminal:

```bash
cat ~/.local/state/hallouminate/daemon-bootstrap.log
```

If the daemon comes up cleanly, the store is healthy — stop here.

## Triage — three mismatches, three repairs

| Error contains | Meaning | Repair |
|---|---|---|
| `store schema stale: found vX, expected vY` | Store written by an older schema than this binary | **None — auto-heals** (below). Never `rm -rf` for this case. |
| `store schema version X is NEWER than this build expects (Y)` | This binary is older than the one that wrote the store | **Upgrade hallouminate** (preferred). Manual sequence only if you must stay on the old binary. |
| `embedding store mismatch: store has (model …, quantized …, embeddings_enabled …)` | The `[embeddings]` config changed since the store was built | **Manual sequence** below. |

### Auto-heal (stale store) — verify, don't delete

On a stale store the daemon renames it to `<ground-dir>.bak-v<found>` and
reindexes every configured corpus during startup — no manual step. Verify
with `hallouminate daemon status` (expect `running`) and a test
`hallouminate ground "<query>"`. The backup is kept for recovery and pruned
automatically after ~30 days (`STALE_BACKUP_MAX_AGE`). If the rebuild itself
fails, the daemon removes the partial store so the next boot retries, and the
backup survives.

## Manual sequence

For a confirmed NEWER-store error you can't upgrade away, or an
embedding-store mismatch.

### Step 1 — Identify the store directory

Both error messages name the store path. If you cannot locate it from the
error output, the default store lives at:

```
~/.local/share/hallouminate/ground
```

This path is overridable via `[storage] ground_dir` in your
`~/.config/hallouminate/config.toml`. Verify:

```bash
hallouminate config show | grep ground_dir
```

Confirm the path is correct before proceeding. Deleting the wrong directory
causes data loss — the ground store is rebuilt from your source wikis, but
verify you know where each corpus lives (`hallouminate config show`) before
running the next step.

### Step 2 — Stop the daemon

```bash
hallouminate daemon stop
```

This sends a graceful shutdown request over the control socket and waits for
the socket file to disappear (up to 10 s). If no daemon is running the command
is a no-op.

### Step 3 — Delete the mismatched store

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

### Step 4 — Restart the daemon

```bash
hallouminate daemon restart
```

This stops any running daemon (if you skipped Step 2) and spawns a fresh one,
then waits until it is reachable.

### Step 5 — Re-run the index

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

## Rules

- Triage first. A `store schema stale` error needs **no** manual repair — the
  daemon auto-rebuilds and keeps a `.bak-v<N>` backup. Do not `rm -rf` for it.
- Only delete the store for a confirmed NEWER-store or embedding-mismatch
  error. Do not run `rm -rf` speculatively.
- For a NEWER-store error, upgrading the binary is the fix; deletion is the
  last resort — it discards an index the newer binary could still read.
- The store is derived data — rebuild time grows with corpus size. On very
  large corpora, plan for the index step to take several minutes.
- If `hallouminate daemon stop` times out (> 10 s), kill the process directly
  (`kill $(lsof -t -- <socket-path>)`) and then delete the socket file before
  proceeding. The socket path is `$HALLOUMINATE_SOCKET` when set, otherwise
  `$XDG_RUNTIME_DIR/hallouminate/daemon.sock`, otherwise
  `~/.cache/hallouminate/daemon.sock`.