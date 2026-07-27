---
status: reviewed
last_verified: 2026-07-15
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/151
  - crates/hallouminate-domain/src/corpus/chunker.rs
---
# Chunk budget and tokenizer

Markdown chunking targets a 384-token budget per chunk, measured by the
same tokenizer the embedding model uses at inference time rather than by
character or word count. The choice of tokenizer over character counting,
and the size of the budget itself, both trace back to keeping what gets
embedded as close as possible to what the model actually sees.[^1]

## Why the production tokenizer, not character counts

A character or word count is a proxy for token count, and a bad one:
markdown headings, inline code, and prose vary widely in
characters-per-token depending on punctuation density and whether text is
prose or a fenced code block. Chunking against a character budget risks
either truncating a chunk mid-sentence well before the model's real limit,
or overshooting it and having the embedder silently truncate the tail of
a chunk it never sees. `Tokenizer`, re-exported at the domain crust rather
than depending on the `tokenizers` crate directly, is the same tokenizer
`hallouminate-adapters::embedder` loads for inference — so a chunk that
fits the 384-token budget at chunk time is guaranteed to fit whole at
embed time.[^2]

## Heading paths as chunk identity

Each chunk carries a `heading_path` — the stack of markdown headings
above it, joined with `::` — alongside its text. This is not cosmetic
metadata: two chunks with identical prose but different heading paths are
different chunks, because the heading path is part of what a search
result renders back to the caller (`# architecture > ## Testing`, for
instance) and part of what makes a chunk citable. A rename of a heading
changes chunk identity even if the underlying prose is untouched, which is
why heading-only edits still trigger re-embedding for the chunks below
that heading.[^3]

## The tension: chunk size vs retrieval precision

384 tokens is a compromise, not a tuned optimum. A smaller budget yields
more, narrower chunks — better precision when a query matches one
specific paragraph, because irrelevant neighboring prose does not dilute
the embedding, but worse when the answer to a query spans two adjacent
paragraphs that end up split across chunks. A larger budget keeps
multi-paragraph context together at the cost of embedding averaging over
more unrelated content, which flattens the vector and makes narrow queries
match less precisely. 384 tokens sits close to a single wiki section —
most `##` sections in this corpus run two to four paragraphs — which is
the shape the chunker was tuned against, not an information-theoretic
ideal.[^4]

## What happens at the boundary

`MarkdownChunker` splits at heading and paragraph boundaries first and
only falls back to a hard token cut inside an unusually long paragraph.
That ordering means a chunk almost never ends mid-sentence in ordinary
prose, even though the hard cap exists as a backstop for pages that don't
follow the wiki's own house style of short paragraphs.[^5]

See [embedding-model-selection](embedding-model-selection.md), [architecture](architecture.md), [design-rationale](design-rationale.md).

[^1]: `crates/hallouminate-domain/src/corpus/chunker.rs:1-40`.
[^2]: `crates/hallouminate-domain/src/corpus/chunker.rs:10-13`; `crates/hallouminate-adapters/src/embedder.rs:14-24`.
[^3]: `crates/hallouminate-domain/src/corpus/chunker.rs:118-131`.
[^4]: https://github.com/paulnsorensen/hallouminate/issues/151
[^5]: `crates/hallouminate-domain/src/corpus/chunker.rs:60-96`.

_Source: issue #151 and `hallouminate-domain::corpus::chunker` · Updated: 2026-07-15_
