---
status: draft
last_verified: 2026-11-01
confidence: speculating
---
# Facts layer extraction evidence

No study compares schema-first and schema-free LLM fact extraction on the same data with 2025-2026 models.
The evidence found shows a trade: a closed schema loses relations that do not fit, and an open schema multiplies relation types.
The evidence comes from [[facts-layer-extraction-supersession-research]]; its confidence is `speculating`.
This page records evidence for fork Q1 only; the decision is open in [[facts-layer-open-forks]].

## Closed-ontology extraction accuracy on 2026 models

Closed-ontology fact extraction was measured on 2026 frontier models in one small study (ACL 2026 KaLLM workshop, `aclanthology.org/2026.kallm-1.21`).
The schema had 6 entity types and 96 Wikidata relation types, tested on 10 DocRED documents.

| Model | Entity F1 | Triple F1 |
| --- | --- | --- |
| Claude Opus 4.5 | not separately reported | 39.9% |
| Gemini 3 Pro | not separately reported | 39.3% |
| GPT-5.1 | 84.0% | 21.7% |
| Mistral 7B | 74.1% | 4.2% |

The authors call strict triple F1 a lower bound.
In a manual review of Gemini 3 Pro output, 61.5% of predicted triples were absent from the gold set, and many were supported by the text.
The gold set also penalized valid synonyms, such as `parent organization` when it expected `part of`.
Smaller models broke the closed schema more often.

## Open extraction and relation sprawl

Open LLM fact extraction without normalization produces close to one relation type per edge, as the KGGen paper (arXiv 2502.09956) reports for GraphRAG and OpenIE.
KGGen clusters entities and relations after extraction.
It scored 66.07% on its MINE-1 benchmark against 47.80% for GraphRAG and 29.84% for OpenIE.
By model, Claude Sonnet 3.5 scored 73%, GPT-4o 66%, and Gemini 2.0 Flash 44%.
A manual check of 100 triples found 98% valid for KGGen, 55% for OpenIE, and 0% for GraphRAG.
KGGen names over- and under-deduplication as remaining problems.
These are the authors' own benchmark and judge, on 2025 models.

## Schema canonicalization trade-off

Schema canonicalization maps freely extracted relations onto a target schema, and the EDC paper (arXiv 2404.03868) measures what it costs.
On WebNLG, canonicalizing to the target schema cut the relation count from 529 to 200, with precision 0.956 against 0.982 for the open graph.
Target-alignment F1 on WebNLG was 0.746, and 0.794 after one refinement round.
These results use GPT-3.5 and GPT-4 era models, and the extracted table was partly garbled.
AutoSchemaKG (arXiv 2505.23628) induces a schema from the corpus and reports triple precision, recall, and F1 above 90% in most cases.
That score uses a Llama-3-8B extractor and a DeepSeek-V3 judge, with no human gold set.

## Extraction cost signals

Facts layer extraction cost is reported in incompatible units across the sources found.
LightRAG makes one extraction call per chunk and updates without a full rebuild, while GraphRAG regenerates its communities.
AutoSchemaKG used about 78,400 GPU hours for three web-scale corpora.
No per-document token cost for schema-first and schema-free extraction on the same corpus was found.

## Labelled inference from the extraction evidence

This section is inference from the extraction evidence, not a decision.
A closed schema misses relations, and its measured loss is partly an artifact of strict scoring.
An open schema needs a canonicalization step, or relation types sprawl.
No source tests a small fixed predicate set with an open fallback.

_Source: [[facts-layer-extraction-supersession-research]] (agent `/briesearch` deep tier, 2026-11-01) · Updated: 2026-11-01_
