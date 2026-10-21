---
status: draft
confidence: speculating
---
# Research: does an RDF or graph store help modern LLMs more than inline fact demarcation?

An agent-run deep-tier `/briesearch` report for the hallouminate repository, published 2026-10-18, supports one finding: explicit fact structure helps LLMs, but the store technology does not carry the gain.
Canonical source: cheese research corpus file `research/rdf-store-vs-inline-facts-llm/rdf-store-vs-inline-facts-llm.md` (local, outside git).
The report passed `ground-check` with 0 errors and `budget-check` with no findings; 27 raw extracts stay in that corpus directory.

## Supported claims of the store-versus-inline report

The store-versus-inline report found no study that compares an RDF store with inline markdown facts on a small wiki with 2025-2026 models.
Its nearest evidence covers five areas.

- Query languages: a 2026 dbt rerun of the data.world benchmark (Claude Sonnet 4.6, GPT-5.3 Codex) raised text-to-SQL from 32.7% to 64.5%. A modeled semantic layer scored 100% in scope and refused out-of-scope questions. That layer is dbt MetricFlow, not RDF.
- Graph retrieval: graph RAG wins multi-hop and temporal questions and ties or loses single-hop questions. A triples-only graph scored 34.28 F1 against 64.78 for plain RAG on NQ.
- Agent memory: LongMemEval found that facts-only memory hurts, but extracted facts as extra index keys over source text raise recall@k by 9.4% and QA accuracy by 5.4%.
- Prompt format: linearized triples beat prose for GPT-4; markdown `key: value` lines beat JSON for GPT-4.1-nano.
- Cost: graph indexing of one 56k-token book costs 330k-655k LLM tokens against 116k for a tree index.

## Relevance of the store-versus-inline report to hallouminate

The store-versus-inline report is the evidence base for [[facts-layer-store-evidence]].
It bears on fork Q2 (where facts live) and fork Q4 (time model) in [[facts-layer-open-forks]].
It records no design decision.

## Limitations of the store-versus-inline report

The store-versus-inline report has `speculating` confidence.
Most rows rest on one source, and the modern-model rows are vendor-run.
Most graph-memory and graph-RAG numbers use gpt-4o-era or smaller models.
It found no 2025-2026 frontier-model scores for text-to-SPARQL or text-to-Cypher, and no measurement on a corpus under 200k tokens.

_Source: cheese research corpus `research/rdf-store-vs-inline-facts-llm/rdf-store-vs-inline-facts-llm.md` · Updated: 2026-10-21_
