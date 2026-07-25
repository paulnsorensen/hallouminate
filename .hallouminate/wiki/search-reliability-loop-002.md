# Search reliability loop ADR 002

## ADR-002: Separate production regression from reranker measurement  [status: accepted]

- **Context:** The existing eval hardcodes quantized BGE embeddings while production defaults to full-precision Snowflake embeddings, and `just eval-measure` runs a ten-variant matrix with four crossencoders. The twelve-query dataset is useful for diagnostics but too small and saturated for an automatic default decision.
- **Decision:** `just eval` derives the production embedding configuration from defaults and schedules only fusion without crossencoding. `just eval-measure` runs exactly that baseline plus `jina-reranker-v1-turbo-en`, the documented opt-in, and writes a diagnostic artifact under `.context/`.
- **Alternatives:** Testing every supported embedding or reranker was rejected because it does not answer the shipped product question. Automatic MRR/latency qualification was rejected because the current dataset cannot justify a precise winner threshold. Removing reranker measurement was rejected because it would leave no disciplined comparison path.
- **Consequences:** Scheduled evaluation becomes faster and production-aligned. Explicit measurement reports quality deltas, per-query changes, cold load, warm p50/p95, timeouts, and failures, but never selects or enables a candidate. Larger judged-set and confidence-policy work remains in issues #150 and #287.[^eval][^policy]

[^eval]: https://github.com/paulnsorensen/hallouminate/issues/150
[^policy]: https://github.com/paulnsorensen/hallouminate/issues/287

_Source: approved search-reliability-loop spec · Updated: 2026-07-24 · Supersedes: ground-search-quality-adrs.md ADR-004 thresholds_