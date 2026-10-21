---
status: draft
last_verified: 2026-10-05
confidence: speculating
---
# Facts layer landscape for LLM wikis

No single standard covers how an LLM wiki compiles information into a facts layer or ontology.
The sources checked in [[llm-wiki-facts-layer-landscape-research]] split into four layers that a design must combine.
This page records that landscape as research evidence; it records no hallouminate design decision.
The open choices live in [[facts-layer-open-forks]].

## Four layers of LLM wiki facts standards

The LLM wiki facts landscape has four layers, and each standard covers only one.

| Layer | What it standardizes | Examples |
| --- | --- | --- |
| Workflow and layout | Where files go and which operations run | Karpathy LLM-wiki gist (raw / wiki / schema; ingest, query, lint; `index.md`, `log.md`), AGENTS.md, Agent Skills, llms.txt |
| Fact syntax in markdown | How one fact looks inside a page | Basic Memory `- [category] text #tag` and `- relation_type [[Target]]`; MCP memory server entity/relation/observation JSONL |
| Formal fact model | How a fact carries qualifiers, sources, and validity | Wikibase statements, RDF 1.2 reifiers, nanopublications, PROV-O, SHACL, SKOS, LinkML, ISO GQL |
| Construction method | How an LLM extracts facts from text | Schema-first or schema-emergent pipelines (next section) |

RDF 1.2 Concepts is a W3C Candidate Recommendation (2026-04-07), not a Recommendation.
Nanopublication guidelines are a working draft.
SHACL, JSON-LD 1.1, and ISO/IEC 39075:2024 (GQL) are final standards.

## Schema-first versus schema-emergent fact extraction

LLM fact extraction pipelines differ mainly on whether a schema constrains extraction.

- **Schema-first** starts from competency questions and a vocabulary: Ontology 101, NeOn, OntoGPT/SPIRES (extracts against a LinkML schema), Ontogenia, and Neo4j SimpleKGPipeline with a manual schema.
- **Schema-emergent** extracts freely and clusters afterwards: Microsoft GraphRAG, KGGen, and LightRAG.
- **LLM-proposed schema, then pruned** sits between them: Neo4j SimpleKGPipeline can let the LLM propose the schema and prune it after extraction.

All LLM-era pipelines share one shape: chunk the source, extract entities and relations per chunk, resolve duplicates, summarize or validate, and keep a link from each fact to its source chunk.
Graphiti adds bi-temporal edges: it keeps episodes as provenance and invalidates a contradicted edge with valid and invalid timestamps.

## Facts layer constructs hallouminate already has

Hallouminate already covers part of the facts-layer landscape without a graph.
Page-level lifecycle frontmatter (`draft | reviewed | trusted | deprecated`) is described in [[wiki-conventions]].
Claim-level marks (`confirmed | qualified | superseded | contradicted`) are described in [[claim-provenance-marks]].
The markdown files are the source of truth and LanceDB is derived, as [[design-rationale]] records.
The research found no existing standard that matches these claim marks exactly.

## Facts layer tooling snapshot (2026-10-05)

The facts-layer research checked Git host release and push dates for maintained tools on 2026-10-05.

- Rust: oxigraph (RDF store and SPARQL, v0.5.11), sophia_rs, horned-owl (OWL), rudof (ShEx and SHACL), nemo (Datalog), reasonable (OWL 2 RL).
- Python: rdflib 7.6.0, pySHACL, linkml (v1.12.0-rc1), ontogpt, graphrag v3.2.0, LightRAG, graphiti, neo4j-graphrag, cognee, instructor.
- Stale or unlicensed: cozo (no push since 2024-12-04), kg-gen and ReLiK (no license).

This snapshot ages quickly; check the source before choosing a dependency.

## Facts layer landscape gaps

The facts-layer landscape research did not cover METHONTOLOGY, eXtreme Design, Text2KG, Obsidian Breadcrumbs or Dataview inline fields.
The Open Knowledge Format (OKF) appears only in comments on the Karpathy gist; no spec was found.
It is not known whether oxigraph, rdflib, or sophia_rs support RDF 1.2 triple terms and reifiers.

_Source: [[llm-wiki-facts-layer-landscape-research]] (agent `/briesearch`, 2026-10-05) · Updated: 2026-10-21_
