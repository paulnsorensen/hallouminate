# Hallouminate implementation tasks

Source of truth for the Ralphify loop. One unchecked item per iteration; one commit per item. Tick (`- [x]`) when done. Add new sub-work in the appropriate phase if discovered. Phases mirror the spec.

## Phase 1 — Index pipeline + ground (docs only) + config + hook installer

### Foundation: manifest, common types, storage

- [x] **Cargo manifest: add Phase 1 dependencies.** Add `tokio` (rt-multi-thread, macros, fs, process), `rusqlite` (bundled), `sqlite-vec`, `fastembed`, `blake3`, `walkdir`, `ignore`, `globset`, `serde`/`serde_json`, `toml`, `thiserror`, `anyhow`, `tracing` + `tracing-subscriber`, `directories`, `shellexpand`, `chrono`, `once_cell`. Pin compatible versions. No code changes; just `cargo build` should succeed with the empty `main.rs` still compiling.
- [x] **`domains/common`: scaffold leaf slice.** Create `src/domains/mod.rs`, `src/domains/common/mod.rs` (or `index.rs` per Sliced Bread crust), and types: `FileRef` (newtype around canonical `PathBuf`), `ChunkId` (i64 newtype), `Corpus` (newtype around `String`), `HallouminateError` (thiserror enum: `Io`, `Db`, `Embed`, `Config`, `Indexer`). Re-export from `common::index`. Wire `pub mod domains;` in `main.rs`. Build green.
- [x] **`adapters/fs`: tilde expansion + canonicalization.** Add `src/adapters/mod.rs` and `src/adapters/fs/mod.rs` exposing `expand_tilde(&str) -> PathBuf` (uses `shellexpand`) and `canonicalize_or_passthrough(&Path) -> FileRef`. Cover with unit tests for `~/foo`, absolute paths, and non-existent paths. No external IO beyond canonicalization.
- [x] **`adapters/sqlite::pool`: connection pool with sqlite-vec auto_extension.** `src/adapters/sqlite/mod.rs` + `pool.rs`. Single-writer pool: open one rusqlite `Connection`, register sqlite-vec via `rusqlite::ffi::sqlite3_auto_extension` (port the `unsafe transmute` pattern from `tern-vectors/src/config.rs` referenced in spec). Provide `open_db(path: &Path) -> Result<Connection>`. Unit test: open a `:memory:` DB and run `SELECT vec_version();`.
- [x] **`adapters/sqlite::schema`: DDL bootstrap.** `schema.rs` with `apply_schema(&Connection)`: CREATE TABLE `files`, `chunks`, virtual table `chunks_fts` (FTS5 porter unicode61, content=chunks, content_rowid=chunk_id), virtual table `chunks_vec` (vec0, embedding FLOAT[384]), plus FTS5 sync triggers. Idempotent (`IF NOT EXISTS` / `CREATE TRIGGER IF NOT EXISTS`). Also create `meta` table holding `(key, value)` rows so embedding model name can live in the DB header. Unit test: apply schema twice on `:memory:`, verify no error.
- [x] **`adapters/sqlite::queries`: file row CRUD.** Functions `upsert_file`, `get_file_by_ref`, `touch_mtime`, `delete_file_cascade`, `all_files_for_corpus`. Use parameterized statements. Unit-test full round-trip on `:memory:` + apply_schema.
- [x] **`adapters/sqlite::queries`: chunk row CRUD + FTS sync verification.** `insert_chunk`, `delete_chunks_for_file`. Unit test: insert chunk, query `chunks_fts MATCH 'word'` and confirm match; delete file, confirm chunks_fts row is gone.
- [x] **`adapters/sqlite::queries`: vec row CRUD.** `insert_vec(chunk_id, &[f32; 384])`, `delete_vec_for_chunk`, `knn_chunks(&query, k)` returning `(chunk_id, distance)` via `vec_distance_cosine`. Unit test: insert two normalized vectors, query, assert ordering.

### Corpus pipeline

- [x] **`domains/corpus::walker`: glob-aware path expansion.** `walker.rs` with `scan(corpus: &CorpusConfig) -> Vec<(FileRef, Mtime)>` using `walkdir` + `globset`. Honor `paths`, `globs`, `exclude` from spec config. Unit test on a tempdir fixture with included/excluded files.
- [x] **`domains/corpus::hasher`: blake3 file hashing.** `hasher.rs` with `blake3_file(&Path) -> Result<String>` (hex). Unit test: hash known content matches `blake3` CLI golden value.
- [x] **`domains/corpus::chunker`: markdown heading-aware chunking.** `chunker.rs` with `chunk_markdown(text: &str) -> Vec<Chunk>` where `Chunk { ord, heading_path: Vec<String>, line_start, line_end, text }`. Split on H1/H2/H3 boundaries, carry heading stack, preserve exact `line_start..=line_end` 1-indexed. Unit tests for: nested headings, code-fence-spanning chunks, file with no heading.
- [ ] **`domains/corpus::summary`: H1 + first paragraph summary, capped 280 chars.** `summary.rs` with `extract_summary(text: &str, fallback_filename: &str) -> String`. Unit tests: H1-present, no-H1 (uses filename), paragraph cap behavior.
- [ ] **`domains/corpus::keywords`: top-8 frequency tokens with stopwords.** `keywords.rs` with `extract_keywords(text: &str) -> Vec<String>`. Strip code fences, lowercase, alphanum-only, filter built-in stopword list, frequency-rank, return top 8. Unit test: deterministic for a fixture.
- [ ] **`domains/corpus::snippet`: 240-char chunk snippet at word boundary.** `snippet.rs` with `make_snippet(text: &str) -> String` collapsing whitespace and truncating at last word boundary <= 240 chars. Unit tests for short, long, multi-whitespace input.
- [ ] **`domains/corpus`: crust facade.** `index.rs` re-exports the public surface (`scan`, `chunk_markdown`, `extract_summary`, `extract_keywords`, `make_snippet`, `blake3_file`, `Chunk`). All siblings stay private to the slice.

### Embeddings

- [ ] **`domains/embeddings`: fastembed wrapper for bge-small-en-v1.5.** `embeddings/index.rs` with `Embedder` struct holding a `TextEmbedding`. Constructor accepts model name (`"bge-small-en-v1.5"` default, `"all-minilm-l6-v2"` opt-in) and cache dir. `embed_batch(&[String]) -> Vec<[f32; 384]>` L2-normalizes each vector. Persist model name into `meta` table on first init; refuse to mix vectors when stored model differs (refuse with clear error pointing at `--reset`). Unit test (gated on env var or `#[ignore]` so CI without model download stays green): same input → bit-identical bytes.

### Indexer

- [ ] **`domains/indexer::plan`: disk-vs-db diff.** `plan.rs` with `plan(disk: Vec<(FileRef, Mtime)>, db: HashMap<FileRef, FileRow>) -> IndexPlan { upserts, mtime_touches, deletes }`. No IO. Unit-test the matrix: new file, unchanged mtime, mtime-changed-but-same-hash placeholder, vanished file.
- [ ] **`domains/indexer::apply`: upsert + cascade-delete pipeline.** `apply.rs` with `apply(plan, &mut Connection, &Embedder, &CorpusConfig)`: re-chunk + re-embed for upserts, `touch_mtime` for mtime-only, `delete_file_cascade` for deletes. Wraps each file in a SQLite transaction. Maintain `embeddings_inserted_total` debug counter via `tracing` for idempotency assertions.
- [ ] **`domains/indexer`: crust facade.** `indexer/index.rs` exposes `index_corpus(&CorpusConfig, &mut Connection, &Embedder) -> IndexStats`. Slice integration test on a 3-file tempdir corpus: first run upserts all, second run reports zero embedding inserts, deleting one file then re-running prunes its rows from `files`/`chunks`/`chunks_fts`/`chunks_vec`.

### Search

- [ ] **`domains/search::fts`: BM25 query over `chunks_fts`.** `fts.rs` with `fts_search(&Connection, query: &str, limit: usize) -> Vec<(ChunkId, f64 /*rank*/)>` using `chunks_fts MATCH ? ORDER BY rank LIMIT ?`. Unit test on seeded fixture: keyword finds expected chunk.
- [ ] **`domains/search::vector`: cosine KNN over `chunks_vec`.** `vector.rs` with `vec_search(&Connection, query_embedding: &[f32; 384], limit: usize) -> Vec<(ChunkId, f64)>` using `vec_distance_cosine`. Unit test on two normalized vectors verifies ordering matches dot-product expectation.
- [ ] **`domains/search::rrf`: Reciprocal Rank Fusion port.** `rrf.rs` with `rrf_fuse(fts: &[(ChunkId, _)], vec: &[(ChunkId, _)], k: u32) -> Vec<FusedHit>` where `FusedHit { chunk_id, score, fts_rank, vec_rank }`. Port the math from `tern-codebase/search.rs`. Unit tests: chunk-only-in-fts, chunk-only-in-vec, chunk-in-both; tunable k changes ordering predictably.
- [ ] **`domains/search::convex`: Convex Combination fusion (α=0.5).** `convex.rs` with `convex_fuse(fts, vec, alpha: f32)`. Unit test: with α=1.0 falls back to FTS ordering; with α=0.0 falls back to vec ordering.
- [ ] **`domains/search`: crust facade.** `search/index.rs` exposes a `search(query, fusion: Fusion, limit) -> Vec<FusedHit>` that delegates to fts+vec+chosen fuse. Hide siblings.

### Ground response orchestration

- [ ] **`domains/ground::types`: response shapes.** `types.rs` with `GroundResponse`, `DocFile`, `DocChunk`, `Stats`, `Warning` matching the spec JSON. Derive `Serialize`. Unit test: snapshot serialize a fixture into the documented shape.
- [ ] **`domains/ground::orchestrate`: assemble per-file rollup.** `orchestrate.rs` with `ground(query, &Connection, &Embedder, opts: GroundOpts) -> GroundResponse`. Computes stats, fuses, groups chunks under their `file_ref`, file-level score = best chunk's RRF score, sorts files, caps to `top_files`, caps chunks per file to `chunks_per_file`. Unit test on seeded fixture.
- [ ] **`domains/ground`: crust facade.** `ground/index.rs` re-exports `ground`, `GroundOpts`, `GroundResponse`.

### Config + CLI + hook installer

- [ ] **`app/config`: TOML config loader + defaults merger.** `app/config/mod.rs` with `Config { corpora: Vec<CorpusConfig>, code_repos: Vec<CodeRepoConfig>, search: SearchConfig, embeddings: EmbeddingsConfig, watch: WatchConfig, storage: StorageConfig }`. `load(path: Option<&Path>) -> Config` resolves XDG (`~/.config/hallouminate/config.toml`), merges defaults from spec. Validate (no empty corpus, valid fusion). Unit-test parsing the spec example block.
- [ ] **`app/cli`: clap-derive subcommands skeleton.** `app/cli/mod.rs` with `Cli { command: Subcommand }` and stub variants `Index`, `Ground`, `Hook { Install | Uninstall }`, `Config { Init | Show }`. Wire `main.rs` to parse + match. Each variant prints "todo" for now. Build + clippy clean.
- [ ] **`app/cli::index`: wire `index` to indexer.** Implement `cmd_index(args)` that loads config, opens DB, runs schema, instantiates Embedder, iterates corpora (or filtered single corpus / `--paths-from`), calls `index_corpus`. Print JSON stats. Integration test (uses real fastembed; mark `#[ignore]` if model download is not available in CI) on a tempdir corpus: verifies row counts.
- [ ] **`app/cli::ground`: wire `ground` to ground orchestration.** Implement `cmd_ground(query, opts)` that loads config, opens DB, instantiates Embedder, calls `ground::ground(...)`, prints JSON (or `--pretty`). Integration test (`#[ignore]`-gated if needed) on the same fixture corpus: query targeting one fixture file is the top hit; chunk's `line_range` matches the source heading.
- [ ] **`app/cli::config`: `config init` and `config show`.** `init` writes a default config.toml to XDG path (refuses overwrite without `--force`). `show` resolves the merged config and pretty-prints TOML. Unit test on a tempdir XDG override.
- [ ] **`app/cli::hook`: `hook install` / `hook uninstall`.** `install` writes `.git/hooks/post-commit` and `.git/hooks/post-merge` invoking `hallouminate index --restrict-to "$PWD"` (or `--all`). Idempotent: reruns leave a single hook entry (delimited by `# hallouminate-managed-block`). `uninstall` strips the managed block. Unit test in a tempdir git repo: install twice, count managed blocks == 1; uninstall, count == 0.
- [ ] **`app/cli::index --restrict-to`.** Add `--restrict-to <path>` flag re-indexing only configured files under the given root. Unit test: restrict to subdir leaves files outside untouched in the DB.

### Acceptance gates

- [ ] **Integration test: fixture corpus end-to-end.** `tests/ground_fixture.rs` writes 3 markdown fixtures, runs `index`, runs `ground`, asserts top hit is the targeted file and that chunk `line_range` matches. Use a real DB on tempdir; embeddings via real fastembed (gate behind feature or env if first-run download is too slow for CI).
- [ ] **Integration test: idempotency.** Re-run `index` on the same fixture; assert `embeddings_inserted_total == 0` on second run.
- [ ] **Integration test: cascade-delete.** Delete a fixture file, re-run `index`, assert `files`/`chunks`/`chunks_fts`/`chunks_vec` rows for that path are gone.
- [ ] **Integration test: fusion config switch.** With `[search].fusion = "convex"` and `"rrf"`, top-1 result for a fixture query is identical.
- [ ] **Integration test: embedding determinism + model-mismatch refuse.** Re-open DB after switching embeddings.model; expect a clear error pointing at `--reset` and no writes.

## Phase 2 — `--include-code` / tilth fan-out

- [ ] **Phase 2 placeholder.** Implement `domains/tilth` (subprocess fan-out with semaphore + timeout + soft-fail), `--include-code` flag on `ground`, soft-warn on empty `[[code_repo]]`, integration test with PATH-stripped tilth.

## Phase 3 — `--watch`

- [ ] **Phase 3 placeholder.** Implement `index --watch` with `notify-debouncer-full` (FileIdMap, ≥500 ms debounce, catch-up scan on start). macOS symlink caveat documented in `--help`.
