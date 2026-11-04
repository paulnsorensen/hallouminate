---
status: draft
last_verified: 2026-10-21
---
# Facts layer open design forks

The hallouminate facts layer has four open design forks, and no decision has been made.
This page lists each fork, its options, and the evidence that bears on it.
The evidence lives in [[facts-layer-landscape]], [[facts-layer-store-evidence]], [[facts-layer-extraction-evidence]], and [[facts-layer-supersession-evidence]].
When a fork is decided, record the decision here with its rationale, or in a new ADR page that links here.

## Fork Q1: how facts are extracted

Fork Q1 asks whether facts layer extraction is schema-first, schema-emergent, or an LLM-proposed schema pruned after extraction.
Schema-first gives stable predicates that can be validated, at a higher setup cost.
Schema-emergent needs no setup, but predicates sprawl and need clustering to remove duplicates.
The pruned middle path is what Neo4j SimpleKGPipeline offers.
On 2026 models, a closed 96-relation schema gave 21.7% to 39.9% triple F1, partly because strict scoring rejects valid synonyms; see [[facts-layer-extraction-evidence]].
No study tests a small fixed predicate set with an open fallback.
Status: open.

## Fork Q2: where facts live

Fork Q2 asks whether facts live inline in wiki markdown, in a sidecar JSONL store, or in a sidecar RDF store.
Inline facts keep markdown as the source of truth, which [[design-rationale]] already requires for the wiki.
A JSONL or RDF sidecar could be compiled from inline facts, the same way LanceDB rows derive from markdown.
An RDF sidecar adds SPARQL and SHACL, plus a dependency such as oxigraph.
The store evidence shows gains from explicit structure and from facts used as extra index keys over source text, not from the store technology.
A metadata prefix inside embedded chunk text beat a separate fused vector in one 2026 study; no study tests inline fact lines against a sidecar.
Status: open.

## Fork Q3: which statement model facts use

Fork Q3 asks which statement model fits: plain triples, Wikibase statements, RDF 1.2 reifiers, or nanopublications.
Plain triples lose provenance, and a triples-only graph lost badly to plain RAG on single-hop questions.
Wikibase statements carry qualifiers, references, and rank, which resemble the existing [[claim-provenance-marks]] statuses.
RDF 1.2 reifiers are a Candidate Recommendation; library support is unknown.
Nanopublications are the heaviest option.
Every statement model checked keeps a changed fact with a status instead of deleting it; see [[facts-layer-supersession-evidence]].
Status: open; the Q2 answer constrains it.

## Fork Q4: how facts change over time

Fork Q4 asks whether the existing supersession claim marks are enough, or whether facts need time qualifiers or bi-temporal edges.
Claim marks already record that one claim supersedes another, with a `ref=` to the replacement.
Graphiti-style bi-temporal edges help only for "what was true at time T" questions.
In the evidence, temporal questions are the one category where graph memory showed a consistent gain, and timestamps on facts raised temporal recall in LongMemEval.
On the 2026 STALE benchmark, unlabelled old and new facts in context gave 15.7% (GPT-5.4) to 55.2% (Gemini-3.1-pro); no study tests a superseded marker against removal.
Status: open.

## Facts layer fork order and next step

The facts layer forks have a dependency order that a design session can follow.
Q2 constrains Q3, and Q1 and Q4 are independent of each other.
A proposed order is Q2, Q3, Q1, Q4; nobody has confirmed it.
The next step is a `/mold` design session that settles the forks with the user.

_Source: [[llm-wiki-facts-layer-landscape-research]], [[rdf-store-vs-inline-facts-research]], [[facts-layer-extraction-supersession-research]] · Updated: 2026-11-01_
