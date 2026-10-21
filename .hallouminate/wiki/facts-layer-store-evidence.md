---
status: draft
last_verified: 2026-10-18
confidence: speculating
---
# Graph store versus inline facts evidence for LLMs

The evidence shows that explicit fact structure helps LLMs, but the store technology does not carry the gain.
No study compares an RDF or graph store with inline markdown fact demarcation on a small wiki with 2025-2026 models.
The evidence comes from [[rdf-store-vs-inline-facts-research]]; its confidence is `speculating`.
This page records evidence only; the decision is open in [[facts-layer-open-forks]].

## Knowledge graph query accuracy on modern models

Knowledge-graph query evidence for LLMs comes mostly from one enterprise benchmark.
In 2023, GPT-4 answered 16% of questions over SQL and 54% over a knowledge-graph representation (arXiv 2311.07509).
Ontology checks plus LLM repair raised the knowledge-graph result to 72% in 2024 (arXiv 2405.11706).
A 2026 dbt rerun of the same benchmark used Claude Sonnet 4.6 and GPT-5.3 Codex.
Text-to-SQL rose from 32.7% to 64.5%, and a modeled semantic layer scored 100% on in-scope questions.
The semantic layer refused out-of-scope questions; text-to-SQL returned plausible wrong numbers.
That semantic layer is dbt MetricFlow, not RDF or SPARQL, and nobody has rerun the 54% knowledge-graph result on modern models.
SPARQL-LLM ranked first of eight systems in the TEXT2SPARQL 2026 challenge, but the abstract gives no absolute accuracy.

## Graph RAG versus plain RAG by question type

Graph retrieval helps some question types and hurts others.

| Question type | Result | Source |
| --- | --- | --- |
| Single-hop (NQ) | Plain RAG 64.78 F1, community GraphRAG 63.01, HippoRAG2 61.03 | arXiv 2502.11371 |
| Single-hop, triples-only graph | 34.28 F1: extraction drops context | arXiv 2502.11371 |
| Multi-hop (MultiHop-RAG) | HippoRAG2 70.27 vs RAG 67.02 | arXiv 2502.11371 |
| Temporal (MultiHop-RAG) | HippoRAG2 49.91 vs RAG 30.70 | arXiv 2502.11371 |
| Simple fact retrieval (GraphRAG-Bench) | RAG matches graph RAG | arXiv 2506.05690 |

These runs use Llama-3.1, Qwen2.5 up to 14B, and GPT-4o-mini; no frontier 2025-2026 model was tested.
Graph indexing of one 56k-token book cost 654,673 LLM tokens for MS-GraphRAG, 329,993 for HippoRAG2, and 115,541 for RAPTOR.

## Agent memory benchmarks and knowledge updates

Agent memory benchmarks test whether stored facts survive change over time.
LongMemEval (ICLR 2025) found that compressing history into facts alone hurts overall accuracy.
Adding extracted facts as extra index keys over the original text raised recall@k by 9.4% and QA accuracy by 5.4%.
Timestamps on facts raised temporal recall by 6.8% to 11.3%.
In the Mem0 paper (gpt-4o-mini), the graph variant gained only on temporal questions, and full context scored highest (72.90).
Zep's graph memory lowered knowledge-update accuracy on gpt-4o-mini (76.9 to 74.4) and raised it on gpt-4o (78.2 to 83.3).
A text-only memory system reports 94.87% on LongMemEval with gpt-5-mini, including 96.2% on knowledge updates; this number is self-reported.
A Letta file agent with semantic search and grep scored 74.0% on LoCoMo against a self-reported 68.5% for Mem0's graph variant.

## Prompt format for facts in context

The format of facts in the prompt still changes LLM accuracy in the evidence found.
Shuffled triples beat human prose for GPT-4 on DocRED (75.48 vs 59.16 EM, 1-hop).
A 2025 test on GPT-4.1-nano ranked markdown `key: value` lines first (60.7%), JSON at 52.3%, and prose at 49.6%.
GPT-4 was more robust to template changes than GPT-3.5, so larger models may care less.
No measured results for Claude 4.x, Gemini 2.5, or GPT-5 on fact formats were found.

## Long context and agentic search for small corpora

Long-context and agentic-search evidence bears on a small wiki that may fit in one prompt.
Chroma's Context Rot report (2025) found that 18 models, including GPT-4.1, Claude 4, and Gemini 2.5, degrade as input grows.
NoLiMa found 11 of 13 models below half their short-context score at 32K tokens.
Anthropic describes Claude Code as hybrid: `CLAUDE.md` loads up front, and glob and grep retrieve files just in time.
No quantified comparison for a corpus under 200k tokens with a frontier model was found.

## Labelled inference from the store evidence

This section is inference from the store evidence, not a decision.
The evidence shows no benefit from RDF or a graph store as such.
It shows benefits from explicit fact structure, source text kept beside extracted facts, refusal on out-of-scope queries, and timestamps for temporal questions.
A sidecar graph has measured value only for multi-hop and temporal queries, at a measured construction cost.

_Source: [[rdf-store-vs-inline-facts-research]] (agent `/briesearch` deep tier, 2026-10-18) · Updated: 2026-10-21_
