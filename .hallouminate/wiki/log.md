# Ingest Log

## Log


- 2026-06-30 · 3f7bfad9660b4f5a · new-page · design-rationale.md · positioning (compiled-memory pattern), filesystem-truth/derived-LanceDB, non-goals (no markdown schema, no code intelligence)
- 2026-06-30 · 3f7bfad9660b4f5a · skipped-near-duplicate · daemon-and-cli.md · cross-version IPC unsupported already documented (lines 54-59)
- 2026-06-30 · 3f7bfad9660b4f5a · skipped-near-duplicate · mcp-surface.md · multi-root writes target first root only already documented (lines 107-110)
- 2026-06-30 · 3f7bfad9660b4f5a · skipped-near-duplicate · wiki-conventions.md · durable-vs-transient split + post-land cadence + query-first already documented
- 2026-06-30 · 3f7bfad9660b4f5a · skipped-near-duplicate · — · embedding-model "doc drift" is a transient task-level observation, not durable wiki knowledge
- 2026-06-30 · 3f7bfad9660b4f5a · skipped-near-duplicate · — · scaling thresholds / vector-store-optional guidance is generic LLM-wiki landscape, not hallouminate-specific (hallouminate always embeds)
- 2026-06-30 · 3f7bfad9660b4f5a · conflict-flagged · — · report states v0.1.0 / Unix-only / no published releases; repo is at v0.2.2 with release automation — maturity snapshot is stale, version status not ingested (lives in Cargo.toml/git tags)


- 2026-07-13 · f564e0b5e8058ad1 · new-page · worktree-corpus-identity.md · cross-worktree index stomping mechanism + root-scoped-deletes direction (#215) + watcher baseline-only caveat
- 2026-07-13 · f564e0b5e8058ad1 · merged · worktree-dev-gotchas.md · pointer section to worktree-corpus-identity (agents hit the trap here first)
- 2026-07-13 · f564e0b5e8058ad1 · merged · daemon-and-cli.md · Socket location gains double-daemon divergence (#218) + cold-start 30s window (#220)
- 2026-07-13 · f564e0b5e8058ad1 · merged · daemon-and-cli.md · Write-lane section gains verified caps/timeouts/ResourceKey model (2026-07-13 audit)
- 2026-07-13 · f564e0b5e8058ad1 · new-page · blocking-inference-offload.md · offload coverage map after #176; inline gaps #217, lock granularity #219
- 2026-07-13 · f564e0b5e8058ad1 · merged · ort-arena-retention.md · idle-exit starvation under fleets (#222) + rerank batch uncapped (#221)



- 2026-07-24 · db6afb639232b5c3 · new-page · ground-search-quality.md · compiled issue #288 draft spec, complete acceptance/non-goal/follow-up/risk set, and implementation locks
- 2026-07-24 · db6afb639232b5c3 · new-page · ground-search-quality-adrs.md · ADR-001…005 with rejected alternatives and research provenance
- 2026-07-24 · db6afb639232b5c3 · new-page · domain-model.md · proposed display/search text split, root-aware corpus identity, and schema-v4 semantics
- 2026-07-24 · db6afb639232b5c3 · merged · worktree-corpus-identity.md · #215 delete safety separated from #288's remaining read-isolation defect; prior rejection of per-root identity superseded
- 2026-07-24 · db6afb639232b5c3 · merged · wiki-conventions.md · self-contained retrieved-section authoring rule
- 2026-07-24 · db6afb639232b5c3 · conflict-flagged · claim-provenance-marks.md · spec word “verbatim” conflicts with current claim-comment stripping; preserved as an implementation lock, not silently blended
- 2026-07-24 · 20e9e2453d821787 · merged · ground-search-quality-adrs.md · RRF magnitude blindness and normalized-fusion/reranking alternatives compiled into ADR-001
- 2026-07-24 · 1d610416fdb5f598 · merged · ground-search-quality-adrs.md · vendor opt-in practice, small-model option, and adaptive-reranking evidence compiled into ADR-004
- 2026-07-24 · 8cdfbe193a3c43ce · merged · ground-search-quality-adrs.md · Sourcegraph BM25F structural-separation precedent and citation-specific uncertainty compiled into ADR-001
- 2026-07-24 · 817f924dcea2f858 · merged · ground-search-quality-adrs.md · Anthropic contextual-retrieval evidence compiled into ADR-005
- 2026-07-24 · 817f924dcea2f858 · merged · wiki-conventions.md · deterministic-context design translated into the self-contained section rule
- 2026-07-24 · a8b939082259c6b1 · new-page · ground-search-evaluation.md · relevance metrics, chunk assertions, latency gate, scheduled workflow, and selected production-readiness evidence
- 2026-07-24 · 1dfc05c09b4025be · merged · ground-search-quality.md · live footnote-inversion and sibling-worktree reproductions retained as the before/after contract
- 2026-07-24 · 1dfc05c09b4025be · conflict-flagged · ort-arena-retention.md · current batch cap contradicts the stale #221 gap; narrow supersession note added while #285 owns full correction
- 2026-07-24 · 1dfc05c09b4025be · skipped-near-duplicate · — · transient session/environment state omitted after durable diagnoses were merged


- 2026-08-16 · wiki-harvest-hallouminate-20260816 · merged · architecture.md · third closed boundary seam: `arrow` now sourced only via `lancedb::arrow::arrow` re-export, not declared directly, after a duplicate-arrow-major build break killed the #306 arrow-v59 bump (#356)
- 2026-08-23 · fix-285-stale-wiki-claims · merged · ort-arena-retention.md · #221 rerank-batch-uncapped gap folded into the mechanism as a landed fix (`crossencoder.rs:56`, `RERANK_BATCH_SIZE = 32`); resolves the 2026-07-24 conflict-flagged narrow note (#285)
- 2026-08-23 · fix-285-stale-wiki-claims · verified-no-change · worktree-corpus-identity.md · already records the #215 delete fix and #290 per-worktree identity re-key with historical framing preserved (landed 2026-07-24); #285's second bullet was already satisfied


- 2026-08-23 · wiki-harvest-hallouminate-20260823 · rewritten · multi-format-ingestion.md · Phase 1 shipped (0.7.0, #380): page was still "markdown-only today" but Format enum + detect_format + HandlerRegistry now route markdown/text/RST/spreadsheet; wiki corpus stays **/*.md, multi-format rides repo:NAME:corpus; prepare_file skips instead of hard-erroring; PDF/office/code still deferred
- 2026-08-23 · wiki-harvest-hallouminate-20260823 · new-page · not-found-suggestions.md · #385 zero-round-trip error enrichment: pick_corpus "did you mean" + read_markdown ancestor listing (cap 20) and strsim closest filenames (cap 3); strsim already transitive via clap
- 2026-08-23 · wiki-harvest-hallouminate-20260823 · merged · mcp-surface.md · read_markdown now defaults corpus to wiki-for-cwd and gained line_numbers/footnotes params; get_footnote removed in 0.7.0 (#384, eleven→ten tools), replaced by footnotes:"only"; not-found enrichment cross-linked
- 2026-08-23 · wiki-harvest-hallouminate-20260823 · merged · index.md · tool count eleven→ten, multi-format gloss refreshed to shipped Phase 1, added not-found-suggestions entry


- 2026-08-30 · wiki-harvest-hallouminate-20260830 · new-adr · supervisor-restart-ladder.md · documented the shared `Ladder<A>` evaluator and its separate churn and supervisor configurations; corrected the crash-loop sequence, distinguished the direct watchdog path, and recorded why restarts do not change heartbeats (#407/#408/#409, closing #387)
- 2026-08-30 · wiki-harvest-hallouminate-20260830 · fixed-stale · blocking-inference-offload.md · every citation still pointed at the pre-#273 `crates/hallouminate/src/daemon/` layout; repointed to `hallouminate-daemon`/`hallouminate-adapters` with corrected line numbers (functions moved, e.g. `run_embedding_blocking` 626→816, real `apply_batch` 939-1009→1168)
- 2026-08-30 · wiki-harvest-hallouminate-20260830 · merged · architecture.md, daemon-and-cli.md, index.md · cross-links to the new supervisor-restart-ladder page

