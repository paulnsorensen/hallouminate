---
name: wiki-query
description: Answer a question from a hallouminate wiki with grounded, cited detail. Use when the user asks something the wiki should know — "what does the wiki say about X", "how does Y work here", "look it up in the wiki", "/wiki-query", or any factual question about a repo whose knowledge lives in a hallouminate corpus. An opus root plans the search and synthesizes the answer; haiku sub-agents fan out one `ground` search per sub-question and return cited evidence. Every claim in the answer carries a `path:line` citation back to the corpus. Do NOT use to write or update wiki entries (use wiki-ingest) or to bootstrap a new wiki (use wiki-init).
---

# wiki-query — cited retrieval from a hallouminate wiki

Answer a question **strictly from the wiki**, with a citation on every claim. The
model is a synthesizer over retrieved chunks, never a substitute for them. If the
corpus does not support a claim, say so — do not fall back to training data.

**Agent topology (required):**

- **Root = opus.** Plans the retrieval, decides what's a distinct sub-question,
  synthesizes the final answer, and verifies every citation. Reasoning lives here.
- **Fan-out = haiku.** One sub-agent per sub-question. Each runs `ground`, reads
  the top chunks, and returns a compact cited evidence digest — never prose for
  the user. Retrieval noise stays in the sub-agent's context, not the root's.

The root NEVER answers from memory of the codebase. It answers from what the
haiku digests bring back.

## Flow

### 1. Plan (root / opus)

- Restate the question and name loaded assumptions in it.
- Decompose into **2–5 orthogonal sub-questions**. One retrieval angle each —
  splitting "how does auth work and where are tokens stored" into two beats one
  blurry search. Single, narrow questions skip decomposition.
- Pick the corpus. If unspecified and >1 corpus exists, call `list_corpora` and
  ask which, or default to the repo's `repo:{name}:wiki`.
- Optionally `list_tree` once to see the wiki's shape — use it to phrase searches
  toward the right area (progressive disclosure: navigate the tree before
  reading leaves).

### 2. Fan out (haiku, parallel)

Spawn one haiku sub-agent per sub-question, **in a single message** so they run
concurrently. Give each the exact `ground` call to make and this contract:

> Run `ground { query: "<sub-question>", corpus: "<corpus>", top_files: 5, chunks_per_file: 3 }`.
> For each chunk that actually bears on the question, return a row:
> `{ claim, path, line_range, heading_path, score, snippet (≤200 chars) }`.
> If `ground` returns nothing relevant, return `{ found: false }` for that
> sub-question. Do NOT paraphrase beyond the snippet. Do NOT answer the user's
> question — you only gather evidence. If a top chunk is truncated and the answer
> hinges on it, `read_markdown` that one file and quote the exact lines.

`ground` returns per file: `summary, keywords, score, mtime, corpus, chunks[]`,
and per chunk: `heading_path` (H1→leaf breadcrumb), `line_range` ([start,end],
1-based), `score`, `snippet`. That is the citation material — pass it up verbatim.

### 3. Synthesize (root / opus)

- Merge the haiku digests. Drop chunks below the relevance others clear; dedup
  overlapping spans.
- Write the answer **lead-first**: the direct answer, then supporting detail.
- **Cite every claim** inline as `` `path:start-end` `` (e.g.
  `` `architecture/dataflow.md:134-198` ``), optionally with the `heading_path`
  breadcrumb for navigation. A sentence with no citation is a bug — either find
  the chunk that backs it or cut it.
- **Calibrate.** Tag the overall answer `certain` (corpus directly states it),
  `partial` (corpus implies it / pieced from multiple chunks), or
  `not in wiki` (no supporting chunk — say this plainly, don't guess).
- If sub-questions came back `found: false`, list them as **gaps** — what the
  wiki doesn't cover yet (hand-off candidates for `wiki-ingest`).

### 4. Verify before answering

For each citation, confirm the cited `line_range` in that file actually contains
the claim — `read_markdown` the file if a claim is high-stakes or a snippet was
truncated. Wrong citations are worse than no citations.

## Output shape

```
**Answer** (<certain|partial|not in wiki>)
<lead-first synthesis, every claim carrying a `path:line` citation>

**Sources**
- `path:line` — <heading_path breadcrumb> — <one-line what it supports>

**Gaps** (omit if none)
- <sub-question the wiki couldn't answer>
```

## Rules

- Ground every claim in a retrieved chunk. No chunk → no claim.
- The root reasons and synthesizes; haiku sub-agents retrieve. Never invert.
- Fan out sub-questions in one message so searches run in parallel.
- Prefer `ground` (hybrid lexical + vector + rerank) over guessing a filename.
- `read_markdown` to confirm exact lines before citing anything critical.
- Say "not in the wiki" out loud rather than answering from training data.
- Do not write to the corpus — this skill is read-only.
