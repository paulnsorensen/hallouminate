# Search reliability loop ADR 001

## ADR-001: Verify authored knowledge through frozen retrieval probes  [status: accepted]

- **Context:** `wiki-ingest` can write structurally valid knowledge that later fails realistic searches, while exact source identity stored only in frontmatter or footnotes is excluded from first-stage `search_text`.
- **Decision:** Durable external research gets one corpus-local source page with a fixed indexed identity spine and a source-appropriate evidence body. The dependent topic page names the source and supported claim in indexed prose. Exact and natural probes are frozen before drafting, run after the write, and rerun unchanged after at most one revision.
- **Alternatives:** A rigid universal source template was rejected because papers, API documentation, issues, and commits need different evidence structures. Fully flexible prose was rejected because title, URL, attribution, and limitations can drift or disappear from retrieval. Post-write query generation was rejected because it can merely echo the finished wording.
- **Consequences:** Exact topic and source-title failures roll back and return `blocked`; ambiguous natural-query failures retain valid content and return `written-with-retrieval-warning`. The skill gains a bounded verification phase but no index-time LLM dependency.

The targeted first repair is Anthropic's *Introducing Contextual Retrieval*: create `sources/anthropic-contextual-retrieval.md` and add indexed attribution from the Ground search ADR.[^anthropic]

[^anthropic]: https://www.anthropic.com/engineering/contextual-retrieval

_Source: approved search-reliability-loop spec · Updated: 2026-07-24 · Supersedes: —_