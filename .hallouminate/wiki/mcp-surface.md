# MCP surface

`hallouminate serve` starts a stdio MCP server. The server is stateless
beyond its tool router and a startup-captured `cwd`; every tool call
dials the local daemon over a Unix domain socket. Since commit
`87a7213`, `serve` auto-spawns the daemon if no instance is up.

## Default corpus

Tool calls that omit `corpus` default to the wiki for the repository
containing the daemon's cwd — `repo:<NAME>:wiki` for the deepest
`[[repository]]` whose `path` is an ancestor of cwd. When cwd doesn't
sit under any configured repo, the daemon falls back to the existing
single-corpus / ambiguity error and the caller must name a corpus
explicitly. This applies to the read-side tools (`ground`, `list_files`,
`list_tree`); the mutating tools (`add_markdown`, `delete_markdown`) and
`read_markdown` still require an explicit `corpus` to avoid accidental
writes to the wrong wiki or ambiguous reads.

## Tools

### `list_corpora`

Returns every corpus the daemon knows about — explicit `[[corpus]]`
entries plus derived `repo:NAME:wiki` and `repo:NAME:corpus` corpora
from `[[repository]]` declarations. No params. Use this first to learn
what's available.

### `list_files`

Returns the files currently visible in a corpus, honoring its
paths/globs/exclude rules. Param: `corpus` (defaults to wiki-for-cwd).
Returns an array of `{path, absolute_path}`.

### `list_tree`

Same files as `list_files`, but grouped into a `{path, absolute_path,
files, subdirs}` tree rooted at the corpus' first configured path.
Subdirs without markdown anywhere beneath them are pruned so the tree
mirrors `list_files`. Param: `corpus` (defaults to wiki-for-cwd). Use
this for progressive disclosure — navigate the wiki tree without
reading every `index.md` first.

### `ground`

Semantic search. Embeds the query with the configured embeddings model
(default `snowflake/snowflake-arctic-embed-s`), retrieves top chunks from LanceDB,
rolls up per-file with breadcrumb context. Params: `query` (required),
`corpus` (defaults to wiki-for-cwd), `top_files`, `chunks_per_file`,
`limit`, `snippet_chars`. Returns a ripgrep-style outline in `content`
and the full structured response in `structuredContent.docs`.

### `add_markdown`

Atomic-write a markdown file to the corpus' first configured root, then
refresh just that file's LanceDB rows. For `repo:*:wiki` corpora, also
walks ancestor directories from the corpus root down to the new file's
parent and rebuilds the link list inside each `index.md` between
`<!-- HALLOUMINATE:INDEX-START -->` and `<!-- HALLOUMINATE:INDEX-END -->`
markers. A missing ancestor `index.md` is scaffolded; prose outside the
markers is preserved verbatim; files without markers are left alone
(the author opted out).

Params: `corpus`, `path`, `content`, `overwrite` (default `false`).
Symlinks and parent-dir escapes are rejected by the sandbox at
`src/domain/corpus/sandbox.rs`.

### `read_markdown`

Read verbatim UTF-8 contents of a file in a corpus. Params: `corpus`,
`path`. Returns the on-disk text, not the chunked index view. Use this
before `add_markdown { overwrite: true }` to inspect current content.

### `delete_markdown`

Unlink a file from the corpus' first root and prune its rows from the
index. Irreversible. For `repo:*:wiki` corpora, also re-walks the
ancestor `index.md`s so they no longer link to the deleted file.
Params: `corpus`, `path`.

### `index`

Bulk (re)build the LanceDB index for one or all corpora. Params:
`corpus` (optional; omit to rebuild every configured corpus). Use this
when files were touched outside hallouminate — `add_markdown`'s
auto-reindex only covers writes that went through the MCP.

### `corpus_stats`

Index health for one corpus: indexed file count, total chunk rows, the newest
index timestamp (`last_indexed_ms`, null when never indexed), and how many
on-disk files matching the corpus globs are not yet indexed. Param: `corpus`
(defaults to wiki-for-cwd, same resolution as `list_files`). `structuredContent`
is `{ corpus, indexed_files, total_chunks, last_indexed_ms, unindexed_files }`.

### `get_footnote`

Resolve a single citation: the footnote target for a page's `#footnote_number`.
Params: `corpus` (defaults to wiki-for-cwd, same as `ground`), `page` (the wiki
page's relative path), `footnote_number` (the label after `^` — `"1"` for `[^1]`,
`"note"` for `[^note]`). Expands one footnote without reading the whole page.

## Error mapping

The MCP transport maps daemon `ErrorKind` variants to JSON-RPC codes:

| Daemon variant | JSON-RPC code | Meaning |
|---|---|---|
| `InvalidParams` | `-32602` | caller-supplied input failures (bad corpus name, unsafe path, missing required arg) |
| `Internal` | `-32603` | server / transport faults |

Anything that fails before the daemon returns a typed envelope
(transport error, decode failure, daemon unavailable) collapses to
`-32603` so MCP clients don't misinterpret a network flake as user
error.

## Multi-root corpora

Writes (`add_markdown`, `delete_markdown`) always target the corpus'
FIRST configured root. Reads (`list_files`, `list_tree`, `read_markdown`,
`ground`) see all roots, but `list_tree` collapses to the first root for
its tree representation. Keep one root if you can — the surprise factor
on writes is the main cost.

## When the daemon is unreachable

Tool calls return `-32603` with the message "daemon unavailable: …".
The MCP server does NOT fall back to opening a local LanceDB handle —
that's exactly the multi-process race the daemon exists to prevent.
