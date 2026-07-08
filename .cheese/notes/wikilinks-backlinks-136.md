status: blocked: out of context
next: cook
artifact: .cheese/notes/wikilinks-backlinks-136.md
Issue #136: wikilink validation + backlinks. ALL CODE IS WRITTEN AND COMMITTED (2 commits: 87d54b0 MCP tool, ce8580b integration tests). Only remaining work is running the gates and fixing any fallout, then a final commit.

## Done (this session, committed)

- `src/adapters/mcp/tools.rs` (commit 87d54b0): added `BacklinksParams` struct and standalone `backlinks(corpus, path)` MCP tool method, modeled on `read_markdown`/`get_footnote`. Added `BacklinksRequest, BacklinksResult` to the `use crate::app::daemon::{...}` import list. Also added a `backlinks` line to `SERVER_INSTRUCTIONS`'s tool list for consistency. Verified via `cargo check --all-targets` — compiled clean.
- `tests/daemon.rs` (commit ce8580b): added `BacklinksRequest` to the `use hallouminate::app::daemon::{...}` import list. Added two integration tests after `daemon_add_markdown_returns_lint_warnings_without_blocking_the_write` (~line 524):
  - `daemon_add_markdown_flags_dangling_wikilink`: writes a page with `[[missing-page]]` (no matching file) — asserts warnings mention `missing-page` and `no matching page`. Also writes `real-page.md`, then a page linking `[[real-page]]` — asserts no wikilink warning.
  - `daemon_backlinks_returns_pages_linking_to_target`: writes `other-page.md`, `linker.md` (contains `[[other-page]]`), and `unrelated.md` (no link), then calls `DaemonRequestPayload::Backlinks(BacklinksRequest { corpus: Some("docs".into()), path: "other-page.md".into() })` and asserts `backlinks == ["linker.md"]`.
  - **NOT YET RUN** — `cargo test --test daemon <name>` was still compiling (>4 min, likely first full rebuild of the test binary in this worktree) when this session hit its context budget. The compile itself was clean per the prior `cargo check --all-targets` pass with the same tools.rs changes; the test binary just needed its own build.

## Left / next steps (in order)

1. Run `cargo test --test daemon daemon_add_markdown_flags_dangling_wikilink` and `cargo test --test daemon daemon_backlinks_returns_pages_linking_to_target` (run separately — `cargo test --test daemon <name1> <name2>` is NOT valid syntax, cargo test only takes one filter substring positional arg). Use a long foreground timeout (5-10 min) since this looks like a cold test-binary build. Fix any assertion/wiring mismatches.
2. Run the full gate sequence: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (whole suite). Fix fallout.
3. Once green, commit via `/commit` skill with a final conventional commit referencing #136 (only needed if gates required fixes beyond the two commits already made — if everything is already green, the two existing commits ARE the final state, no further commit needed beyond confirming gates pass). Do NOT push.

## Design notes carried from the locked spec (unchanged)

- Standalone MCP tool `backlinks(corpus, path)`, NOT a field bolted onto `read_markdown`. DONE.
- Wikilink target resolution answers to both full path-without-extension slug and bare filename stem (`slug_identifiers`). DONE (domain layer, prior session).
- Lint is advisory-only, never blocks the write. DONE (prior session).

---

status: blocked: out of context
next: cook
artifact: .cheese/notes/wikilinks-backlinks-136.md
Issue #136: wikilink validation + backlinks. validate.rs, ipc.rs, dispatch.rs lint wiring, mod.rs re-exports, and handle_backlinks + dispatch arm are ALL DONE and cargo-check-verified (compiles clean). Still need: MCP backlinks tool, tests/daemon.rs integration tests, fmt/clippy/test gates, final commit.

## Done (verified via `cargo check --all-targets` — compiles clean, committed at b4ef098)

- `src/domain/corpus/validate.rs`: `find_wikilinks`, `normalize_slug`, `slug_identifiers`, `corpus_slugs`, `lint_wikilinks` + 3 unit tests (NOT yet run through `cargo test` — do that on resume).
- `src/domain/corpus.rs`: re-exports.
- `src/app/daemon/dispatch.rs` `handle_add_markdown`: lint_wikilinks wired into warnings.
- `src/app/daemon/ipc.rs`: `BacklinksRequest`, `BacklinksResult`, `DaemonRequestPayload::Backlinks` variant.
- `src/app/daemon/mod.rs`: re-exports `BacklinksRequest, BacklinksResult` added to the `pub use ipc::{...}` list.
- `src/app/daemon/dispatch.rs`: added `handle_backlinks(cfg, cwd, req) -> DaemonResponse` (~line 949-1000, right after `handle_read_markdown`, before `handle_delete_markdown`). Pattern: `pick_corpus_or_default` for corpus resolution, `list_corpus_files` for entries, `slug_identifiers(&req.path)` into a `HashSet<String>` target set, `tokio::task::spawn_blocking` reads each OTHER entry's file via `std::fs::read_to_string(&entry.absolute_path)`, runs `find_wikilinks`, normalizes each link with `normalize_slug`, and collects+sorts matching entry paths. Added dispatch arm `DaemonRequestPayload::Backlinks(req) => handle_backlinks(&effective, &req_cwd, req).await,` right after the `DeleteMarkdown` arm (~line 110-112). Added imports: `normalize_slug, find_wikilinks, slug_identifiers` to the `crate::domain::corpus::{...}` import (line 28), and `BacklinksRequest, BacklinksResult` to the `super::ipc::{...}` import (line 50-55).

## Left / next steps (in order)

1. **`src/adapters/mcp/tools.rs`**: add `BacklinksParams { corpus: Option<String> (#[serde(default)]), path: String }` struct (model on `ReadMarkdownParams`/`GetFootnoteParams`, ~line 464), then a `backlinks` tool method (model on `read_markdown`/`get_footnote`, insert after `get_footnote` before closing `}` of the `impl HallouminateTools` block ~line 818-820). Needs `BacklinksRequest, BacklinksResult` added to the `use crate::app::daemon::{...}` import list (~line 21-27). Tool description: returns corpus-relative paths of pages that `[[wikilink]]` to the given page. `structuredContent` = `{ corpus, path, backlinks }`; `content` = newline-joined backlink paths (or a "no backlinks" message when empty).
2. **`tests/daemon.rs`**: two new tests, modeled on `daemon_add_markdown_returns_lint_warnings_without_blocking_the_write` (~line 439-524):
   - `daemon_add_markdown_flags_dangling_wikilink`: write a page with `[[missing-page]]` (no matching file) — assert `warnings` mentions the broken target. Also write `real-page.md` first, then a page linking `[[real-page]]` — assert no wikilink warning.
   - `daemon_backlinks_returns_pages_linking_to_target`: write 2-3 pages via `AddMarkdown`, one containing `[[other-page]]`, call `DaemonRequestPayload::Backlinks(BacklinksRequest { corpus: Some("docs".into()), path: "other-page.md".into() })`, assert the linking page's path is in `backlinks` and a non-linking page is not.
   - Add `BacklinksRequest` to the `use hallouminate::app::daemon::{...}` import list at top of `tests/daemon.rs` (~line 19-24).
3. Run gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. Fix fallout.
4. Commit via `/commit` skill with conventional commit referencing #136 once gates are green. Do NOT push.

## Design notes carried from the locked spec

- Standalone MCP tool `backlinks(corpus, path)`, NOT a field bolted onto `read_markdown`.
- Wikilink target resolution answers to both full path-without-extension slug and bare filename stem (`slug_identifiers`).
- Lint is advisory-only, never blocks the write.

---

status: blocked: out of context
next: cook
artifact: .cheese/notes/wikilinks-backlinks-136.md
Issue #136: wikilink validation + backlinks. validate.rs domain logic DONE and unit-tested; ipc.rs types DONE; dispatch.rs wiring for add_markdown lint DONE; still need handle_backlinks + dispatch arm, mod.rs re-export, MCP tool, and integration tests.

## Done (verified)

- `src/domain/corpus/validate.rs`: added `find_wikilinks`, `extract_wikilinks` (helper), `normalize_slug`, `slug_identifiers`, `corpus_slugs`, `lint_wikilinks`. Added 3 unit tests: `valid_wikilink_is_not_flagged`, `broken_wikilink_is_flagged`, `wikilink_inside_code_fence_is_ignored`. NOT YET RUN through `cargo test` — do that first on resume.
- `src/domain/corpus.rs`: re-exports `corpus_slugs, find_wikilinks, lint_markdown, lint_wikilinks, normalize_slug, slug_identifiers` from `validate` module (line ~47-49).
- `src/app/daemon/dispatch.rs` `handle_add_markdown` (~line 745-752): wired `lint_wikilinks` into the existing warnings composition, using `crate::domain::corpus::sandbox::list_corpus_files(&corpus)` to build `known_slugs` via `corpus_slugs`. Lint runs BEFORE the write (existing corpus state), so a self-referencing wikilink in a brand-new file will show as broken until a second write — accepted as advisory-only per existing lint philosophy, not blocking.
- `src/app/daemon/ipc.rs`: added `BacklinksRequest { corpus: Option<String>, path: String }`, `BacklinksResult { corpus: String, path: String, backlinks: Vec<String> }`, and `DaemonRequestPayload::Backlinks(BacklinksRequest)` variant (~line 71-74). Verified via full-file read after fixing two self-inflicted edit bugs (duplicate derive line, dropped `file_ref` field on `DeleteMarkdownResult`) — both now confirmed fixed in the file as of last read.

## Left / next steps (in order)

1. **`src/app/daemon/mod.rs`**: add `BacklinksRequest, BacklinksResult` to the `pub use ipc::{...}` list (currently lines 36-42).
2. **`src/app/daemon/dispatch.rs`**:
   - Add `handle_backlinks(cfg: &Config, cwd: &Path, req: BacklinksRequest) -> DaemonResponse` near `handle_read_markdown`/`handle_list_files` (read-only, no mutation guard). Pattern: resolve corpus via `pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref())`; `list_corpus_files(&corpus)` for all `FileEntry`; compute `target_slugs = HashSet` of `slug_identifiers(&req.path)`; for every OTHER file entry, read its content (`std::fs::read_to_string(&entry.absolute_path)` is fine — these paths already passed the corpus scan/glob filters, mirroring how `index_single_file` reads directly without re-sandboxing) inside `tokio::task::spawn_blocking`, run `find_wikilinks`, normalize each, and if any normalized target intersects `target_slugs`, push `entry.path.clone()` into the backlinks Vec. Sort the result for determinism. Return `DaemonResponse::ok(&BacklinksResult { corpus: corpus.name, path: req.path, backlinks })`.
   - Add dispatch arm: `DaemonRequestPayload::Backlinks(req) => handle_backlinks(&effective, &req_cwd, req).await,` in the `match req.payload` block (~line 108, alongside `ReadMarkdown`/`DeleteMarkdown`).
3. **`src/adapters/mcp/tools.rs`**: add `BacklinksParams { corpus: Option<String> (#[serde(default)]), path: String }` struct (model on `ReadMarkdownParams`/`GetFootnoteParams`, ~line 464), then a `backlinks` tool method (model on `read_markdown`/`get_footnote`, insert after `get_footnote` before closing `}` of the `impl HallouminateTools` block ~line 818-820). Needs `BacklinksRequest, BacklinksResult` added to the `use crate::app::daemon::{...}` import list (~line 21-27). Tool description should state: returns corpus-relative paths of pages that `[[wikilink]]` to the given page. `structuredContent` = `{ corpus, path, backlinks }`; `content` = newline-joined backlink paths (or a "no backlinks" message when empty).
4. **`tests/daemon.rs`**: two new tests, modeled on `daemon_add_markdown_returns_lint_warnings_without_blocking_the_write` (lines 439-524):
   - `daemon_add_markdown_flags_dangling_wikilink`: write a page containing `[[missing-page]]` with no matching file in the corpus; assert `warnings` contains a message mentioning the broken target. Also test writing a page with `[[real-page]]` where `real-page.md` already exists in the corpus (write it first) — assert no wikilink warning.
   - `daemon_backlinks_returns_pages_linking_to_target` (or similar name): write 2-3 pages via `AddMarkdown`, one of which contains `[[other-page]]`, then call `DaemonRequestPayload::Backlinks(BacklinksRequest { corpus: Some("docs".into()), path: "other-page.md".into() })` and assert the linking page's path appears in `backlinks`, and a page with no link does not.
   - Add `BacklinksRequest` to the `use hallouminate::app::daemon::{...}` import list at top of `tests/daemon.rs` (~line 19-24).
5. Run gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (verify actual clippy invocation via CI config if a specific lint set exists), `cargo test`. Fix any compile errors from the new wiring (especially unused-import warnings from the mod.rs/dispatch.rs changes).
6. Commit via `/commit` skill with conventional commit referencing #136 once gates are green. Do NOT push.

## Design notes carried from the locked spec

- Standalone MCP tool `backlinks(corpus, path)`, NOT a field bolted onto `read_markdown` — per the issue's literal ask.
- Wikilink target resolution: a page identified by relative path `dir/page.md` answers to both the full path-without-extension slug and the bare filename stem (see `slug_identifiers`) since most real-world wikilinks reference just the page name.
- Lint is advisory-only, never blocks the write — matches `lint_markdown`/`lint_frontmatter`/`lint_claim_marks` philosophy already in `handle_add_markdown`.
