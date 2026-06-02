# hallouminate

[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/paulnsorensen/hallouminate/badge)](https://scorecard.dev/viewer/?uri=github.com/paulnsorensen/hallouminate)

A markdown corpus indexer for LLMs to build and query their own per-repo
wikis. Hallouminate stores markdown verbatim on disk, embeds it with
fastembed, indexes the embeddings in LanceDB, and exposes a small MCP
surface (`add_markdown` / `read_markdown` / `delete_markdown` / `ground`)
so an LLM can author and search a per-repo knowledge base without
leaving its agent loop.

The filesystem is the source of truth; LanceDB rows are derived and
refreshed automatically when an LLM writes via `add_markdown`, or in
bulk via `hallouminate index`. Code files (`.rs`, `.toml`, …) can also
be indexed as text for semantic search, but hallouminate does no
structural analysis — it's a wiki indexer that happens to tolerate
code, not a code intelligence tool.

A long-lived local daemon owns the LanceDB ground directory, per-corpus
mutation locks, and config resolution. The CLI and the stdio MCP server
both talk to it over a Unix domain socket — one owner, no cross-process
LanceDB races.

> 📖 **Full documentation:** <https://cheeselord.dev/hallouminate/>

## Usage

`hallouminate serve` starts the stdio MCP server (auto-spawning the daemon if
none is running) — this is what an MCP client launches:

```sh
hallouminate serve
```

From a source checkout, run subcommands through cargo:

```sh
cargo run -- serve                       # stdio MCP server
cargo run -- index                       # bulk (re)index every configured corpus
cargo run -- ground "how does the daemon work"   # CLI semantic search
cargo run -- config show                 # print the effective merged config
```

## Build

```sh
cargo build --release
```

The binary lands in `target/release/hallouminate`.

## MCP

`hallouminate serve` starts a stdio MCP server. Tools:

- `ground` — semantic search.
- `index` — bulk (re)build a corpus index.
- `list_corpora` — list every configured corpus.
- `list_files` — flat list of relative paths in a corpus.
- `list_tree` — the same files grouped into a directory tree, for
  progressive disclosure without reading every `index.md`.
- `add_markdown` — write a markdown file under the corpus' first root,
  atomic and no-symlink-follow, with auto-reindex of just that file.
  Returns advisory lint `warnings` (empty-destination links, empty mermaid
  blocks, heading-level jumps) without blocking or rewriting the content.
- `read_markdown` — verbatim UTF-8 file contents. Use before overwriting.
- `delete_markdown` — unlink the file and prune its rows from the index.
- `globalize_markdown` — copy an entry into the global corpus to share it
  across repos.

Markdown content is stored verbatim — hallouminate imposes no schema.
Convention for LLM wiki authors: one topic per file, first line `# Title`,
file stem matches the slug.

## Config

The config lives at `$XDG_CONFIG_HOME/hallouminate/config.toml`
(`~/.config/hallouminate/config.toml` by default).

- `hallouminate config init` — scaffold a baseline config.
- `hallouminate config show` — print the effective merged config for the
  current working directory (baseline + repo layer).
- `hallouminate config validate` — parse and flag unknown top-level keys.
- `hallouminate config download` — pre-fetch the configured embedding model
  so the first `index` doesn't pay the download cost.

## FAQ

### How do I turn embeddings off?

Dense embeddings are **on by default**, using the
`snowflake/snowflake-arctic-embed-s` model. On first index hallouminate
downloads that model and fuses its vector signal with lexical search.

To run lexically only — full-text search + ripgrep + rerank, no embedding
model downloaded (just the tokenizer used for chunking) — set `enabled = false`
in `~/.config/hallouminate/config.toml`:

```toml
[embeddings]
enabled = false
```

Changing the embedding mode (or model) for a ground directory that was already
indexed under a different mode trips the store's mismatch guard on the next
run. Delete the ground directory and re-run `hallouminate index` to rebuild:

```sh
rm -rf ~/.local/share/hallouminate/ground
hallouminate index
```

### Which embedding models are supported?

Set `embeddings.model` in your config to one of these (all embed to 384-dim
vectors). Omitting `embeddings.model` selects the default.

| Model | Notes |
| --- | --- |
| `snowflake/snowflake-arctic-embed-s` | **Default.** English, symmetric retrieval. |
| `BAAI/bge-small-en-v1.5` | English, symmetric retrieval. |
| `intfloat/multilingual-e5-small` | Multilingual, asymmetric retrieval; no quantized variant. |

## Skill pack

A Claude Code skill pack ships in this repo under
[`plugins/hallouminate`](plugins/hallouminate). It installs hallouminate and
bootstraps your first wiki for you:

```text
/plugin marketplace add paulnsorensen/hallouminate
/plugin install hallouminate@hallouminate
/hallouminate:install
```

`/install` runs `cargo install hallouminate`, registers the MCP server, then
asks where and how your first wiki should live (Socratic style) before
scaffolding, indexing, and committing it with git. The `release-skills`
workflow publishes versioned skill-pack archives to GitHub Releases.

## License

MIT — see [LICENSE](LICENSE).
