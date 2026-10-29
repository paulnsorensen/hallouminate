---
status: draft
confidence: speculating
---
# Research: methods to compile information into an ontology / facts layer for an LLM wiki

An agent-run `/briesearch` report for the hallouminate repository, published 2026-10-05, supports one finding: no single standard covers an LLM wiki end to end.
Canonical source: cheese research corpus file `research/llm-wiki-ontology-facts-layer/llm-wiki-ontology-facts-layer.md` (local, outside git).
Its raw extracts and manifest stay in that corpus directory.

## Supported claims of the facts-layer landscape report

The facts-layer landscape report splits the evidence into four layers.

1. Workflow and layout: the Karpathy LLM-wiki gist (raw sources, LLM-owned wiki, schema file; ingest, query, lint; `index.md` and an append-only `log.md`). AGENTS.md, Agent Skills, and llms.txt standardize containers and precedence, not facts.
2. Fact syntax in markdown: Basic Memory observations (`- [category] text #tag`) and relations (`- relation_type [[Target]]`); the MCP memory server entity, relation, and observation JSONL model.
3. Formal fact models: Wikibase statements (main snak, qualifiers, references, rank), RDF 1.2 triple terms with reifiers (Candidate Recommendation, 2026-04-07), nanopublications (working draft), PROV-O, SHACL and JSON-LD 1.1 (W3C Recommendations), SKOS, LinkML, and ISO GQL (ISO/IEC 39075:2024).
4. Construction methods: schema-first (competency questions, Ontology 101, NeOn, OntoGPT/SPIRES, Ontogenia, Neo4j SimpleKGPipeline) and schema-emergent (GraphRAG, KGGen, LightRAG). Graphiti adds bi-temporal edge validity.

The report also finds one shared pipeline shape in LLM-era tools: chunk, extract, resolve duplicates, validate or summarize, and keep a source link per fact.

## Relevance of the landscape report to hallouminate

The landscape report maps the existing hallouminate constructs onto the four layers.
Page lifecycle frontmatter and [[claim-provenance-marks]] already give page-level and claim-level provenance.
The report gives the evidence base for [[facts-layer-landscape]] and the open forks in [[facts-layer-open-forks]].

## Limitations of the landscape report

The landscape report has `speculating` confidence overall.
Its "no end-to-end standard" conclusion is an absence claim limited to the sources checked.
It did not cover METHONTOLOGY, eXtreme Design, Text2KG, Obsidian Breadcrumbs or Dataview, or the Open Knowledge Format (OKF), which appears only in gist comments.
Tool maintenance data is a snapshot from 2026-10-05.

_Source: cheese research corpus `research/llm-wiki-ontology-facts-layer/llm-wiki-ontology-facts-layer.md` · Updated: 2026-10-21_
