# MCP surface

`hallouminate serve` starts a stdio MCP server. It is stateless beyond its
tool router and a startup-captured working directory; every tool call dials
the local daemon over a Unix domain socket, and `serve` auto-spawns the daemon
if none is up.

## Default corpus

Read-side tools (`ground`, `list_files`, `list_tree`) that omit `corpus`
default to the wiki for the repository containing the daemon's working
directory — `repo:<NAME>:wiki` for the deepest `[[repository]]` whose `path`
is an ancestor of the cwd. When the cwd sits under no configured repo, the
caller must name a corpus explicitly.

The mutating tools (`add_markdown`, `delete_markdown`) and `read_markdown`
**always** require an explicit `corpus`, to avoid accidental writes to the
wrong wiki or ambiguous reads.

## The nine tools

### `list_corpora`

Every corpus the daemon knows about — explicit `[[corpus]]` entries plus
derived `repo:NAME:wiki` and `repo:NAME:corpus` corpora. No params. Call this
first to learn what's available.

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
(required), `corpus`, `top_files`, `chunks_per_file`, `limit`, `snippet_chars`.
Returns a ripgrep-style outline in `content` and the full structured response
in `structuredContent.docs`.

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

### `globalize_markdown`

Copy a wiki entry into the global corpus so it can be shared across repos.

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
