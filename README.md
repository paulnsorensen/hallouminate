# hallouminate

[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/paulnsorensen/hallouminate/badge)](https://scorecard.dev/viewer/?uri=github.com/paulnsorensen/hallouminate)
<!-- Register at https://www.bestpractices.dev/en/projects/new, then replace <id> and uncomment:
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/<id>/badge)](https://www.bestpractices.dev/projects/<id>)
-->

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

## Usage

```sh
cargo run -- --name Cheese
```

## Install

```sh
# Prebuilt binary, no Rust toolchain required:
npx hallouminate --help

# Or from crates.io:
cargo install hallouminate
```

The npm package is a thin shim — its postinstall step downloads the
matching prebuilt binary for your platform from the GitHub release.

## Build

```sh
cargo build --release
```

The binary lands in `target/release/hallouminate`.

## MCP

`hallouminate serve` starts a stdio MCP server. Tools:

- `ground` — semantic search.
- `index` — bulk (re)build a corpus index.
- `list_corpora`, `list_files` — discovery.
- `add_markdown` — write a markdown file under the corpus' first root,
  atomic and no-symlink-follow, with auto-reindex of just that file.
  Returns advisory lint `warnings` (empty-destination links, empty mermaid
  blocks, heading-level jumps) without blocking or rewriting the content.
- `read_markdown` — verbatim UTF-8 file contents. Use before overwriting.
- `delete_markdown` — unlink the file and prune its rows from the index.

Markdown content is stored verbatim — hallouminate imposes no schema.
Convention for LLM wiki authors: one topic per file, first line `# Title`,
file stem matches the slug.

## Config

The config lives at `$XDG_CONFIG_HOME/hallouminate/config.toml`
(`~/.config/hallouminate/config.toml` by default). Bootstrap with
`hallouminate config init`, check with `hallouminate config validate`.

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

## License

MIT — see [LICENSE](LICENSE).
