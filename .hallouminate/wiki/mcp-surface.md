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
`limit`, `snippet_chars`, `footnotes`. Returns a ripgrep-style outline in
`content` and the full structured response in `structuredContent.docs`.

`footnotes` (`include` default / `exclude` / `only`, defined at
`crates/hallouminate-domain/src/footnotes.rs`) controls footnote visibility in
snippets only — it is a display filter, not a retrieval one. First-stage
retrieval already runs on footnote-stripped `search_text`, so `exclude` does
not change which chunks come back, only what you are shown; `only` is the
cheap way to pull a page's citation targets without reading the page.

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
`crates/hallouminate-domain/src/corpus/sandbox.rs`.

#### Surgical edit modes

`add_markdown` is not whole-file-only. Three mutually exclusive optional
params switch `content` from "the whole file" to "the fragment", each
requiring the file to already exist
(`crates/hallouminate-daemon/src/dispatch.rs::handle_add_markdown`):

| Param | `content` means | Failure modes |
|---|---|---|
| `under_heading` (+ `position`: `append` default / `prepend`) | text spliced into that heading's section, matched on rendered heading text at any level | heading not found; heading ambiguous |
| `replace_lines` `{start, end}` (1-based, inclusive) | the replacement body for that range | out of range; `start > end` |
| `replace_match` | the replacement for the UNIQUE literal occurrence | 0 matches (not found); >1 (ambiguous) |

Prefer these over `overwrite: true` for a targeted change — re-sending a
whole page to fix one paragraph is how concurrent authors clobber each
other, and the ambiguity errors are the point: they refuse rather than
guess. All three ignore `overwrite`; they compose the new file in memory
and then take the whole-file write path, so the rest of the contract
(atomic write, sandbox, ancestor `index.md` rebuild) is identical.

#### Advisory lints on write

Every write runs four lints over the **composed** file — markdown
structure, frontmatter, `<!--claim:-->` marks, and `[[wikilink]]`
resolution (`lint_markdown` / `lint_frontmatter` / `lint_claim_marks` /
`lint_wikilinks`). They are advisory only: warnings ride back in the
response and the write still lands verbatim. This is deliberate — a lint
that blocked the write would make the tool refuse content it cannot
actually prove wrong, and an author who is mid-refactor would be unable
to land a page that links to a sibling not yet written. See
[wiki-conventions](wiki-conventions.md) for the wikilink rules the lint
enforces. If listing the corpus fails, the wikilink lint is skipped with
a log warning rather than failing the write.

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

### `backlinks`

Return the corpus-relative path of every page that links to a given page via
a `[[wikilink]]`. Params: `corpus` (defaults to wiki-for-cwd), `path`.
`structuredContent` is `{corpus, path, backlinks, warnings}`.

Backlinks are computed by a **live filesystem scan**, not from the LanceDB
index (`crates/hallouminate-daemon/src/dispatch.rs::handle_backlinks` reads
every corpus file on a `spawn_blocking` thread). So the answer reflects the
tree as it is on disk right now, even for pages edited outside the MCP and
never reindexed — the one tool whose result cannot be stale relative to the
files. The cost is O(corpus) reads per call; don't poll it.

Resolution mirrors `add_markdown`'s wikilink lint (both go through
`resolve_slug` in `crates/hallouminate-domain/src/corpus/validate.rs`), so
the two never disagree about what a target names. Asking for
`guides/setup.md` matches both `[[guides/setup]]` and the bare `[[setup]]`
— unless some other page also has the stem `setup`, in which case the bare
form is ambiguous and only the full-path form counts. Targets are matched
case-insensitively with `.md` stripped, `[[target|alias]]` matches on the
target, and links inside fenced code blocks are ignored so an example in a
doc isn't mistaken for a real link. A file the scan cannot read produces a
`warnings` entry and a partial result rather than an error.

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

A multi-root corpus is **read- and search-only**. `add_markdown` and
`delete_markdown` no longer silently pick the first root — `require_single_root`
(`crates/hallouminate-daemon/src/dispatch.rs`) rejects the mutation at request
time with `InvalidParams` naming the root count. Writing to whichever root
happened to be listed first was the worse failure: it looked like it worked.
Config validation was deliberately left alone, so a multi-root corpus still
loads fine — it just refuses mutations.

Reads resolve across **every** root, in `corpus.paths` order, returning the
first root under which the relative path is an existing regular file reached
without traversing a symlinked component (`resolve_read_root` in
`crates/hallouminate-domain/src/corpus/sandbox.rs`). This closed a split
surface where `ground` and `list_files` walked all roots but `read_markdown`
resolved only `paths[0]`, so a file could be searchable yet unreadable. The
probe is non-mutating — unlike the write path it never creates intermediate
directories, so probing a root that lacks the file leaves it untouched. A
symlinked component is a hard stop surfaced immediately, not a reason to try
the next root.

`list_tree` is still first-root-only: its tree is keyed on the first
configured root, so files under `paths[1..]` are dropped from the tree even
though `list_files` and `ground` see them.

Keep one root if you can. Everything above is the cost of not doing so.

## When the daemon is unreachable

Tool calls return `-32603` with the message "daemon unavailable: …".
The MCP server does NOT fall back to opening a local LanceDB handle —
that's exactly the multi-process race the daemon exists to prevent.
