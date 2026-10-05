---
title: "MCP surface"
---

`hallouminate serve` starts a stdio MCP server. It is stateless beyond its
tool router; every tool call dials the local daemon over a Unix domain socket,
and `serve` auto-spawns the daemon if none is up.

Every tool call requires an absolute `cwd` for the active checkout. The server
validates and canonicalizes `cwd` before it loads configuration or contacts the
daemon. It does not use the server startup directory, MCP roots, or a session
working directory as a fallback.

## Corpus scoping

`ground` with no `corpus` searches **every** effective corpus and merges the
results into a single ranked set. The wiki for the repository containing the
request's `cwd` is the priority corpus: its pages win score ties and are ranked
first. Every hit carries its own source corpus, and passing `corpus` explicitly
pins the search to that one corpus.

The other read-side tools (`read_markdown`, `list_files`, `list_tree`,
`backlinks`, `corpus_stats`) that omit `corpus` default to the wiki for the
repository containing `cwd` — `repo:<NAME>:wiki` for the deepest
`[[repository]]` whose `path` is an ancestor of `cwd`. When `cwd` sits under no
configured repo, the caller must name a corpus explicitly.

The mutating tools (`add_markdown`, `delete_markdown`) **always** require an
explicit `corpus`, to avoid accidental writes to the wrong wiki.

## The ten tools

### `list_corpora`

Every corpus the daemon knows about — explicit `[[corpus]]` entries plus
derived `repo:NAME:wiki` and `repo:NAME:corpus` corpora. Param: `cwd` (required
absolute path). Call this first to learn what's available.

### `list_files`

The files currently visible in a corpus, honoring its paths/globs/exclude
rules. Param: `corpus` (defaults to wiki-for-cwd). Returns an array of
`{path, absolute_path}`.

### `list_tree`

The same files as `list_files`, grouped into a `{path, absolute_path, files,
subdirs}` tree. Subdirs with no markdown beneath them are pruned. Use this for
progressive disclosure — navigate the wiki without reading every `index.md`
first. Param: `corpus` (defaults to wiki-for-cwd).

### `ground`

Semantic search. Embeds the query with the configured embedding model
(default `snowflake/snowflake-arctic-embed-s`), retrieves top chunks from
LanceDB, and rolls up per-file with breadcrumb context. Params: `query`
(required), `corpus`, `top_files`, `chunks_per_file`, `limit`, `snippet_chars`,
`footnotes`, `match`, `group_by`, `output`.
Returns a ripgrep-style outline in `content` and the full structured response
in `structuredContent.docs`.

Set `match: "phrase"` for an exact lookup, such as a canonical URL or a
source title. Phrase mode uses bounded case-insensitive literal matching of
the whole query against chunk body text. The match ignores the heading
breadcrumb and the file summary, so inline markup in a heading, such as
`## Tax **Abatement**`, does not match as a phrase. It does not use BM25,
vector search, or the reranker. Stopwords, punctuation, quotes, `%`, and `_`
match literally. Each run of whitespace, such as a line break, a tab, repeated
spaces, or a non-breaking space, counts as one space in the query and in the
chunk text. A phrase that wraps across a hard line break therefore matches.
Hits rank by occurrence count, and `score` is that count.
The query must hold 1 to 512 characters and must not be only whitespace.
The `limit`, `chunks_per_file`, `top_files`, and 10,000-match scan cap can
restrict results. A `phrase-truncated` warning reports these limits. Do not
conclude absence or uniqueness from a truncated result.

Plain-text, JSON, and PDF chunks repeat about 12 percent of the chunk budget from the
previous chunk. A short phrase across a split point is then whole in one
chunk. Phrase mode counts each occurrence once, in the first chunk that holds
it.

Set `group_by: "page"` to return one entry for each PDF page (`page:N`
breadcrumb). The entry is the best chunk of the page, and `chunk_count` gives
the number of matched chunks on that page. `chunks_per_file` then caps pages.
Chunks without a page breadcrumb stay single entries.

Set `output: "counts"` with `match: "phrase"` to get coverage counts.
Each matched file carries `coverage: {chunks, pages}` and no chunks, snippets,
summary, or keywords. `pages` is `null` for a file without page breadcrumbs.
Counts ignore `top_files`, `chunks_per_file`, and `limit`. The 10,000-match scan
cap applies. A counts response lists at most 2,000 files and adds a
`counts-truncated` warning when more files match. A response over the IPC frame
limit returns an error. `output: "counts"` with `match: "ranked"` is rejected.

### `add_markdown`

Atomic-write a markdown file to the corpus' first configured root, then refresh
just that file's LanceDB rows. For `repo:*:wiki` corpora it also rebuilds the
link list inside each ancestor `index.md` between the
`<!-- HALLOUMINATE:INDEX-START -->` / `<!-- HALLOUMINATE:INDEX-END -->`
markers — scaffolding a missing `index.md`, preserving prose outside the
markers, and leaving marker-less files alone. Params: `corpus`, `path`,
`content`, `overwrite` (default `false`). Symlinks and parent-dir escapes are
rejected by the sandbox. Returns advisory lint `warnings` (empty-destination
links, empty mermaid blocks, heading-level jumps) without blocking the write.

### `read_markdown`

Verbatim UTF-8 contents of a file in a corpus. Params: `corpus`, `path`. Use
this before `add_markdown { overwrite: true }` to inspect current content.

### `delete_markdown`

Unlink a file from the corpus' first root and prune its rows from the index.
Irreversible. For `repo:*:wiki` corpora it also re-walks the ancestor
`index.md`s so they no longer link to the deleted file. Params: `corpus`,
`path`.

### `index`

Bulk (re)build the LanceDB index for one or all corpora. Param: `corpus`
(optional; omit to rebuild every configured corpus). Use this when files were
touched outside hallouminate.

### `corpus_stats`

Index health statistics for one corpus: number of indexed files, total chunk
row count, newest index timestamp (`last_indexed_ms`, null when the corpus has
never been indexed), and how many on-disk files matching the corpus globs are
not yet indexed. Param: `corpus` (defaults to wiki-for-cwd, same resolution as
`list_files`). `structuredContent` is `{ corpus, indexed_files, total_chunks,
last_indexed_ms, unindexed_files }`.

### `backlinks`

Corpus-relative paths of every page that links to the given page via a
`[[wikilink]]`. Params: `corpus` (defaults to wiki-for-cwd, same as `ground`),
`path` (the target page's relative path). `structuredContent` is `{ corpus,
path, backlinks }`; `content` is a newline-joined list of backlink paths, or a
message noting there are none. Use this to find which pages reference a page
before renaming or deleting it.

## Migrating from the startup-cwd contract

Pass the active checkout as `cwd` on every request, including `list_corpora`:

```json
{"name":"list_corpora","arguments":{"cwd":"/workspaces/project"}}
{"name":"ground","arguments":{"cwd":"/workspaces/project","query":"release process"}}
```

Use corpus-relative document paths. Resolve each `globs` pattern from every
configured corpus root, not from the daemon startup directory or the filesystem
root:

```toml
[[corpus]]
name = "docs"
paths = ["/workspaces/project/docs"]
globs = ["guides/**/*.md"]
```

## Conventions for LLM authors

Markdown is stored verbatim — hallouminate imposes no schema. The convention
the indexer counts on:

- **One topic per file.** The chunker splits on H1/H2/H3 headings.
- **First non-blank line is `# Title`.** The H1 is the breadcrumb root for
  every chunk and the gloss in the parent `index.md` link list.
- **File stem matches the slug** — lowercase, kebab-case, `.md`.
- **Idempotent writes** — `add_markdown` rejects existing files unless
  `overwrite: true`; `read_markdown` first so you don't clobber blind.

## Error mapping

| Daemon variant | JSON-RPC code | Meaning |
|---|---|---|
| `InvalidParams` | `-32602` | Caller input failures (bad corpus name, unsafe path, missing arg). |
| `Internal` | `-32603` | Server / transport faults, including "daemon unavailable". |

When the daemon is unreachable, calls return `-32603` — the MCP server does
**not** fall back to opening a local LanceDB handle, since that's exactly the
multi-process race the daemon exists to prevent.
