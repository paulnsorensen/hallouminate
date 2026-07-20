# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

GitHub release notes for this project ship only install/download links, not
change descriptions, so entries below are condensed from merged PR titles
for each release window.

## [0.5.0](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.5.0) - 2026-07-19

### Added

- FM-Index as an additive exact-substring search signal
- Daemon: reindex only on real content change (mtime + blake3 gate)
- Daemon: starvation-free maintenance with debt-graduated backpressure
- Daemon: supervise long-lived tasks with a watchdog and boot backoff
- Daemon: status introspection, churn escalation, and supervision wiring

### Changed

- Extracted `hallouminate-daemon` and `hallouminate-config` crates
- Daemon: share one exponential-backoff curve across supervisor and watchdog

### Fixed

- Daemon: ignore access events in the watcher pending-set

## [0.4.1](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.4.1) - 2026-07-16

### Changed

- Docs: local-link corpus rule + skill-improver audit fixes

### Fixed

- Daemon: probe sibling socket before auto-spawning

## [0.4.0](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.4.0) - 2026-07-16

### Added

- Configurable crossencoder rerank timeout
- Daemon: trace maintenance lifecycle
- Daemon: pressure-aware, configurable LanceDB maintenance
- Plugin support for major agent harnesses

### Changed

- Split into domain and adapter crates; sealed the adapter/domain APIs and
  the tokenizer boundary with `cargo-deny`
- Established the application and MCP boundaries

### Fixed

- Daemon: per-request-class client timeouts and rmcp tool annotations
- Indexer: scope deletes to `corpus.paths` and offload embedding via
  `block_in_place`
- Search: bound crossencoder rerank ONNX batch size
- Daemon: retry cold-start connection polling
- Daemon: bound tracing log size and coalesce watcher failures

## [0.3.2](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.3.2) - 2026-07-12

### Fixed

- Race-free npm publishing (idempotent nightly job + custom release-pipeline
  publish job)
- Unbroke `release.yml` startup and the nightly smoke assertion

## [0.3.1](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.3.1) - 2026-07-12

### Added

- npm: `npx` install shim with prebuilt binary download
- Corpus: validate `[[wikilinks]]` and expose a `backlinks` tool
- Release: nightly npx channel and OIDC trusted publishing

### Fixed

- Resolved six bugs from a bug-hunt sweep across the CLI, indexer, and daemon
- Daemon: bound crossencoder rerank with a per-request timeout, plus a
  fusion fallback
- Config: validate unknown keys in every config layer
- Ground: render results by relevance instead of path order
- Search: kill the ripgrep child when the result limit stops the drain
- Daemon: close a watcher symlink TOCTOU by reading corpus files no-follow
- Corpus: close sandbox symlink races and surface partial results
- Lance: pin ground-store ownership to the store itself with a single-owner
  flock
- Completed remediation of a whole-repo bug hunt (per-request resources,
  IPC hardening)
- Capped the bootstrap log, hash-checked bulk indexing, bounded embed
  batches

## [0.3.0](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.3.0) - 2026-07-08

### Added

- Daemon: replace session-eviction with process idle-exit

### Changed

- Ground: embed the union query once and stop cloning hits
- Lance: add store maintenance and a latch text-index guard

### Fixed

- Search: treat `rg` exit 1 with no hits as an empty result, not an error
- Daemon: offload blocking work, timeout idle reads, prune stale backups

## [0.2.4](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.2.4) - 2026-07-07

### Fixed

- Config: per-repository wiki root override + warn on unknown nested keys

## [0.2.3](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.2.3) - 2026-07-05

### Added

- Corpus: claim-level provenance marks
- Ground: corpus-relative path, stale-index detection, and score-semantics
  docs
- Crossencoder: pre-warm the reranker on config download, with progress
  display
- Daemon: auto-rebuild the ground store on schema-version mismatch
- Ground: per-query z-score normalization alongside score
- MCP: full `add_markdown` edit API — heading splice / line-range /
  text-match
- MCP: `corpus_stats` tool for corpus index health
- Wiki-ingest: 3-layer dedup + append-only `log.md` + `index.md` routing
- Daemon: evict the idle embedder to release ORT arena memory
- Indexer: format-aware multi-format ingestion (Phase 1)

### Changed

- Removed MCP client-roots support from tools

### Fixed

- Config: worktree-aware config discovery
- CI: don't let the skills-pack release steal `/latest` from the binary
  release

## [0.2.1](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.2.1) - 2026-06-14

### Fixed

- Marketplace: resolve plugin source to `./plugins/hallouminate`

## [0.2.0](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.2.0) - 2026-06-13

### Added

- MCP: footnote citations — authoring guidance + footnote-aware grounding
- Corpus: optional page-level YAML frontmatter (lifecycle + provenance)
- Cross-harness installable plugin
- Skills: `wiki-reindex` skill
- Ground: cross-repo union ground from a parent directory

### Changed

- Docs: mdBook documentation site; README/wiki accuracy fixes; restyled
  README/docs
- Docs: lead install instructions with the prebuilt-binary installer
- Removed the `globalize_markdown` tool from the MCP

### Fixed

- Daemon: degrade to baseline-only when cwd is above all repos
- Indexer: skip a missing corpus root with a warning instead of aborting
  the run

## [0.1.3](https://github.com/paulnsorensen/hallouminate/releases/tag/v0.1.3) - 2026-06-02

Initial public release.

### Added

- Core corpus slice: walker, hasher, chunker, summary, keywords, snippet
  extraction
- Storage/embedding backend: SQLite adapter, later swapped for LanceDB +
  text-splitter
- Embeddings + indexer pipeline; per-file rollup orchestrator + TOML config
  loader
- `clap`-derive CLI: `index`, `ground`, `config`, `hook` subcommands
- MCP server: `add_markdown` (with auto-indexing), `read_markdown`,
  `delete_markdown`, `config validate`
- IPC daemon so MCP tools share one LanceDB handle; watcher, lifecycle
  commands, wiki globalize
- Repo-level `.hallouminate/config.toml` auto-discovery
- Weighted RRF reranker biased toward FTS
- Wiki defaults: default to repo wiki, `list_tree`, auto-maintained
  `index.md`
- Dense embeddings, opt-in at first, then defaulted on with the
  snowflake-arctic model; curated 384-dim model menu
- Config: warn on an unregistered `.hallouminate/wiki/` in `show`/`validate`
- Markdown linting on `add_markdown`, returning advisory warnings
- Daemon: version-skew self-heal, multi-root reads
- Skill pack: hallouminate skill pack + release-skills workflow; wiki
  lifecycle skills (init, query, ingest)
- MCP: `line_numbers` option for `read_markdown`

### Changed

- Sandbox: migrated from raw libc syscalls to rustix, then to cap-std
- Daemon: hardened dispatch path validation, mtime handling, and telemetry
- Cached ensured search indexes; hardened docs, style, telemetry

### Fixed

- Embeddings: use canonical HuggingFace repo ids for embedding and
  tokenizer loading
- Wiki: skip YAML frontmatter when reading the `index.md` H1 gloss
- Daemon: surface a serialize failure as an Internal error, not a null
  success
- MCP: resolve daemon cwd from client roots
- Sandbox: log the canonicalize fallback; restore explicit `0o755` mode on
  intermediate dir creates
- CI: pin `scorecard-action` to v2.4.3
- Release: install protoc for binary builds; idempotent re-runs; fix the
  ort/ONNX binary build; literal-match the crates.io publish guard; native
  arm runner for aarch64-linux
