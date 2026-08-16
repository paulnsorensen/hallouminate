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

