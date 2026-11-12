---
status: draft
last_verified: 2026-11-01
confidence: speculating
---
# Facts layer supersession evidence

Wikibase, nanopublications, and Graphiti keep a superseded fact and mark it; none deletes it.
The fetched RDF 1.2 sections show no supersession mechanism.
On 2026 models, unlabelled old and new facts in context lead to wrong answers, but no study tests whether a marker fixes this.
The evidence comes from [[facts-layer-extraction-supersession-research]]; its confidence is `speculating`.
This page records evidence for forks Q3 and Q4 only; the decisions are open in [[facts-layer-open-forks]].

## How statement models mark a changed fact

Fact statement models handle a changed value, such as a Lance store schema going from version 3 to 5, by keeping the old statement with a status.

| Model | Change mechanism | Source |
| --- | --- | --- |
| Wikibase statements | Rank: preferred (most up-to-date, used by default), normal (correct, shown less), deprecated (unreliable or known to contain errors) | Wikibase DataModel |
| Nanopublications | Never edited; a new nanopub states the retraction, and retractions can be queried | nanopub docs |
| Graphiti (Zep) | Four timestamps per edge; an LLM finds a contradicting edge and sets the old edge's `t_invalid`; new information wins | arXiv 2501.13956 |
| RDF 1.2 reifiers | A reifier points to an unasserted triple term; time semantics were not found in the fetched sections | W3C RDF 1.2 Concepts |

RDF 1.2 Concepts was a Candidate Recommendation Snapshot on 2026-04-07.
Hallouminate claim marks already record `superseded` with an optional `ref=` pointer (page path, footnote label, or URL), as [[claim-provenance-marks]] describes.

## STALE benchmark results for outdated memories

The STALE benchmark (arXiv 2605.06527, May 2026) tests whether LLM agents notice that a stored memory is no longer valid.
Plain models received the whole chronological dialogue, up to 150K tokens.

| Model | Overall |
| --- | --- |
| Gemini-3.1-pro | 55.2% |
| Qwen3.5-27B | 31.3% |
| Gemini-3.1-flash-lite | 22.4% |
| GPT-5.4 | 15.7% |
| GPT-5.4-nano | 9.8% |
| GPT-4o-mini | 8.7% |

Models find a stale fact when asked directly, but follow a query that assumes the old state.
Gemini-3.1-pro scored 92.0% on direct state questions and 30.0% on queries with an outdated premise; Qwen3.5-27B scored 76.0% and 4.0%.
Conflicts that propagate through a dependency were harder than direct conflicts for nearly all systems.
On a gpt-4o-mini backbone, only LightMem (17.8%) beat the plain model (8.7%); Zep scored 6.0% and mem-0 8.3%.
STALE tested no Claude models, and Gemini-3.1-flash-lite was the judge.

## Graph memory on knowledge-update questions

Graph memory results on knowledge-update questions depend on the model.
In the Zep paper, LongMemEval knowledge-update accuracy rose from 78.2% to 83.3% with gpt-4o and fell from 76.9% to 74.4% with gpt-4o-mini.
The same table shows temporal-reasoning gains of 45.1% to 62.4% (gpt-4o) and 36.5% to 54.1% (gpt-4o-mini).
These are vendor-authored results on gpt-4o era models.

## Labelled inference from the supersession evidence

This section is inference from the supersession evidence, not a decision.
Wikibase, nanopublications, and Graphiti keep old facts with a status, which the existing claim marks already do.
STALE shows that unlabelled history fails, but it does not test labelled history or removal.
Outdated-premise queries fail even for the best model, so a storage choice alone may not fix them.

_Source: [[facts-layer-extraction-supersession-research]] (agent `/briesearch` deep tier, 2026-11-01) · Updated: 2026-11-01_
