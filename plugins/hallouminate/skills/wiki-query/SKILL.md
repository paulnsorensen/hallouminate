---
name: wiki-query
description: Answer a question from a hallouminate wiki with grounded, cited detail. Use when the user asks something the wiki should know — "what does the wiki say about X", "how does Y work here", "look it up in the wiki", "/wiki-query", or any factual question about a repo whose knowledge lives in a hallouminate corpus. Every claim in the answer carries a `path:line` citation back to the corpus. Do NOT use to write or update wiki entries (use wiki-ingest) or to bootstrap a new wiki (use wiki-init).
---

# wiki-query — cited retrieval from a hallouminate wiki

Answer a question **strictly from the wiki**, with a citation on every claim. The
model is a synthesizer over retrieved chunks, never a substitute for them. If the
corpus does not support a claim, say so — do not fall back to training data.

**Agent topology (required):**

- **Root = opus-tier** (the strongest model the harness offers). Plans the
  retrieval, decides what's a distinct sub-question,
  synthesizes the final answer, and verifies every citation. Reasoning lives here.
- **Fan-out = haiku-tier** (the cheapest model). One sub-agent per sub-question. Each runs `ground`, reads
  the top chunks, and returns a compact cited evidence digest — never prose for
  the user. Retrieval noise stays in the sub-agent's context, not the root's.

The root NEVER answers from memory of the codebase. It answers from what the
haiku digests bring back.

Every hallouminate MCP tool call requires the absolute `cwd` of your active checkout.
Keep that `cwd` and the selected corpus fixed throughout each sub-question, including all recovery and citation reads.
Never substitute a main-checkout alias or another corpus to obtain hits.

## Flow

### 1. Plan (root / opus)

- Restate the question and name loaded assumptions.
- Decompose into **2–5 orthogonal sub-questions**, with one retrieval angle each.
  Single, narrow questions skip decomposition.
- Pick the corpus. If unspecified and multiple corpora exist, call
  `list_corpora { cwd }` and ask which, or select the repository's `repo:{name}:wiki`.
- Pass the selected corpus explicitly on every retrieval call.
- Optionally call `list_tree { cwd, corpus }` once to phrase searches.
  Share this tree with the sub-agents; it consumes their tree-listing allowance.

### 2. Fan out (haiku, parallel)

Spawn one haiku sub-agent per sub-question in a single message so they run concurrently.
Give each the exact Ground call, fixed checkout and corpus, and this contract:

> Run `ground { query: "<sub-question>", corpus: "<corpus>", top_files: 5, chunks_per_file: 3, cwd }`.
> A cold corpus makes the first Ground wait for the first reconciliation, up to `search.cold_wait_ms` (default 10 s, maximum 60 s).
> After that wait, Ground can return lexical line-window hits instead of indexed chunks.
> A fallback hit has a `chunk_id` that starts with `lexical-fallback:` and an empty `heading_path`; its `stale` flag does not show index freshness.
> Inspect `index-coverage`, `index-reconciliation`, every `lexical-fallback*` warning (`lexical-fallback`, `-truncated`, `-timeout`, `-failed`, `-scan-timeout`), and `config-path-outside-repo` for the selected corpus before interpreting results, including zero hits.
> Foreign-corpus hits do not establish coverage or successful grounding of the selected repository.
> For each relevant selected-corpus chunk, return:
> `{ claim, path, line_range, heading_path, score, snippet (≤200 chars) }`.
> Use the corpus-relative `path`, not the absolute key in `docs`.
> Do not paraphrase beyond the snippet or answer the user.
> For a truncated supporting chunk, read its file with `read_markdown { cwd, corpus, path, line_numbers: true }`.
> Quote the exact supporting numbered lines.
> Return `{ found, incomplete_retrieval, warnings, remaining_warnings, evidence }`.
> Carry selected-corpus readiness warnings verbatim, including their checkout or root information.
> Set `incomplete_retrieval: true` while coverage is incomplete or reconciliation remains unresolved.
> Also set it when any `lexical-fallback*` warning is present: those results are not evidence of absence.
> `found: false` means no supporting evidence was retrieved; it does not by itself establish a wiki gap.

Ground provides per-file `summary, keywords, score, mtime, corpus, chunks[]`.
Each chunk provides `heading_path`, 1-based `line_range`, `score`, and `snippet`.
Pass citation material unchanged.

#### Bounded recovery (per sub-question)

If evidence is insufficient:

1. Allow at most one `corpus_stats { cwd, corpus }` call.
2. If file coverage is complete, allow at most one identical Ground retry, with every original argument unchanged.
3. Inspect the retry's warnings and carry any remaining reconciliation warning as unresolved.
   Complete file coverage does not prove content freshness or reconciliation completion.
   A stats result alone never clears a reconciliation warning.
4. If evidence remains insufficient, call `list_tree { cwd, corpus }` once, unless the root already supplied that tree.
5. Read up to five relevant pages with `read_markdown { cwd, corpus, path, line_numbers: true }`.
   Count pages, not batch calls; use the same checkout and corpus.
6. Stop recovery and return supporting file citations plus any remaining retrieval limitation.

The recovery budget is one stats call, one identical Ground retry, one tree listing, and five fallback page reads.
Do not poll, loop, call `index`, change configuration, or write a wiki page.
These limits do not restrict normal citation verification reads.

### 3. Synthesize (root / opus)

- Merge the evidence digests. Drop irrelevant chunks and deduplicate overlapping spans.
- Lead with the direct answer, then supporting detail.
- Cite every knowledge claim as `path:start-end`, optionally with its heading breadcrumb.
  Use supporting file content from fallback reads just as you use Ground chunks.
- Tag the answer `certain` for directly stated evidence or `partial` for implied or incomplete evidence.
- Use `not in wiki` and authoring **gaps** only with complete retrieval or direct file evidence supporting that absence.
  Limit any absence conclusion to the scope actually checked.
  Five unsuccessful fallback reads do not establish a corpus-wide absence.
- For complete, warning-free searches, retain the existing citation and gap-reporting behavior.
- Never turn incomplete retrieval alone into `not in wiki`, an authoring gap, or a `wiki-ingest` recommendation.
- Report remaining limitations separately, even when direct file reads answer the question.
  Identify the selected corpus and the tool warnings; do not invent file citations for tool status.

### 4. Verify before answering

Confirm that each cited range contains the claim.
For critical claims or truncated snippets, read the file with
`read_markdown { cwd, corpus, path, line_numbers: true }`.
Use its 1-based gutters rather than counting lines by hand.

## Worked cases

| Input | Disposition and bounded action |
| --- | --- |
| Zero hits; selected corpus has 0/42 files indexed | Return incomplete retrieval, not a wiki gap. Check stats once; use the same-corpus tree and up to five pages if needed. |
| Foreign hits; selected corpus is unindexed | Do not claim local grounding succeeded. Preserve local warnings and recover only within the original checkout and corpus. |
| One stats call shows complete file coverage | Retry the identical Ground call once if evidence is insufficient. Retain any reconciliation warning; coverage alone does not clear it. |
| Reconciliation remains pending | Report the limitation, even if fallback file content supports an answer. Stop after the bounded reads; do not poll. |
| Fully covered, warning-free search | Cite supporting chunks normally. Report unsupported sub-questions as scoped gaps under the existing evidence rules. |

## Output shape

```
**Answer** (<certain|partial|not in wiki>)
<lead-first synthesis, every knowledge claim carrying a path:line citation>

**Sources**
- path:line — <heading breadcrumb> — <what it supports>

**Retrieval limitations** (omit if none)
- <selected corpus, remaining tool warnings, checked scope, and unresolved sub-question>

**Gaps** (omit unless absence is supported)
- <sub-question and scope supported by complete retrieval or direct file evidence>
```

## Rules

- Ground every knowledge claim in a retrieved chunk or supporting file content.
- The root reasons and synthesizes; haiku sub-agents retrieve. Never invert.
- Fan out sub-questions in one message so searches run in parallel.
- Prefer Ground over guessing a filename.
- Confirm exact lines before citing critical claims.
- State unsupported evidence without answering from training data.
- Keep retrieval read-only and preserve its checkout and corpus.

## Discipline

**Iron Law:** No wiki-absence conclusion from incomplete retrieval alone.

**Red flags:** Zero hits become a gap; foreign hits replace local evidence; complete file coverage hides pending reconciliation.

| Rationalization | Why it fails | Required action |
| --- | --- | --- |
| "No hits means no knowledge." | Retrieval can be incomplete. | Inspect warnings and use bounded recovery. |
| "The main checkout has hits." | Another checkout changes the evidence scope. | Keep the original checkout and corpus. |
| "All files are indexed, so reconciliation finished." | Coverage does not establish freshness. | Preserve remaining reconciliation warnings. |
