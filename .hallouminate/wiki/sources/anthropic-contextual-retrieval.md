# Introducing Contextual Retrieval

Anthropic's engineering article by Daniel Ford, published September 19, 2024, describes Contextual Retrieval as a method for improving retrieval by prepending chunk-specific explanatory context before embedding and BM25 indexing.
Canonical source: [Introducing Contextual Retrieval](https://www.anthropic.com/engineering/contextual-retrieval)

## Contextual Retrieval

Anthropic describes Contextual Retrieval as two coordinated techniques: contextual embeddings and contextual BM25. The approach adds explanatory context that identifies a chunk's place in the larger document before semantic and lexical retrieval. Anthropic reports a 49% reduction in top-20 retrieval failures when contextual embeddings and contextual BM25 are combined, and a 67% reduction when reranking is added.[^1]

## Relevance to Hallouminate

This evidence supports Hallouminate's search_text representation: a heading breadcrumb and file summary provide retrieval context before embedding and BM25 indexing, while display text remains the user-facing source evidence. Hallouminate implements a credential-free approximation of the retrieval principle rather than Anthropic's LLM-generated, chunk-specific context.

## Limitations

Anthropic's reported results come from its own experiments across datasets, embedding models, retrieval strategies, and metrics. They support testing contextual retrieval, not a product-specific ranking guarantee for Hallouminate. The full method generates roughly 50–100 tokens of chunk context with an LLM, which remains outside this implementation.

_Source: https://www.anthropic.com/engineering/contextual-retrieval · Updated: 2026-07-24_

[^1]: https://www.anthropic.com/engineering/contextual-retrieval
