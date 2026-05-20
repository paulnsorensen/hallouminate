# MCP surface

`hallouminate serve` starts a stdio MCP server. The server is stateless
beyond its tool router and a startup-captured `cwd`; every tool call
dials the local daemon over a Unix domain socket. Since commit
`87a7213`, `serve` auto-spawns the daemon if no instance is up.

## Tools

### `list_corpora`
Returns every corpus the daemon knows about — explicit `[[corpus]]`
entries plus derived `repo:NAME:wiki` and `repo:NAME:corpus` corpora
from `[[repository]]` declarations. No params. Use this first to learn
what's available.

### `list_files`
Returns the files currently visible in a corpus, honoring its
paths/globs/exclude rules. Param: `corpus` (required when more than one
corpus is configured). Returns an array of `{path, absolute_path}`.

### `ground`
Semantic search. Embeds the query with the configured embeddings model
(default `BAAI/bge-small-en-v1.5`), retrieves top chunks from LanceDB,
rolls up per-file with breadcrumb context. Params: `query` (required),
`corpus`, `top_files`, `chunks_per_file`, `limit`, `snippet_chars`.
Returns a ripgrep-style outline in `content` and the full structured
response in `structuredContent.docs`.

### `add_markdown`
Atomic-write a markdown file to the corpus' first configured root, then
refresh just that file's LanceDB rows. Params: `corpus`, `path`,
`content`, `overwrite` (default `false`). Symlinks and parent-dir
escapes are rejected by the sandbox at `src/domain/corpus/sandbox.rs`.

### `read_markdown`
Read verbatim UTF-8 contents of a file in a corpus. Params: `corpus`,
`path`. Returns the on-disk text, not the chunked index view. Use this
before `add_markdown { overwrite: true }` to inspect current content.

### `delete_markdown`
Unlink a file from the corpus' first root and prune its rows from the
index. Irreversible. Params: `corpus`, `path`.

### `index`
Bulk (re)build the LanceDB index for one or all corpora. Params:
`corpus` (optional; omit to rebuild every configured corpus). Use this
when files were touched outside hallouminate — `add_markdown`'s
auto-reindex only covers writes that went through the MCP.

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
FIRST configured root. Reads (`list_files`, `read_markdown`, `ground`)
see all roots. Keep one root if you can — the surprise factor on
writes is the main cost.

## When the daemon is unreachable

Tool calls return `-32603` with the message "daemon unavailable: …".
The MCP server does NOT fall back to opening a local LanceDB handle —
that's exactly the multi-process race the daemon exists to prevent.
