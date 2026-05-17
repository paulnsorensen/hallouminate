# hallouminate

A bare-bones Rust CLI tool.

## Usage

```sh
cargo run -- --name Cheese
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
- `list_corpora`, `list_files` — discovery.
- `add_markdown` — write a markdown file under the corpus' first root,
  atomic and no-symlink-follow, with auto-reindex of just that file.
- `read_markdown` — verbatim UTF-8 file contents. Use before overwriting.
- `delete_markdown` — unlink the file and prune its rows from the index.

Markdown content is stored verbatim — hallouminate imposes no schema.
Convention for LLM wiki authors: one topic per file, first line `# Title`,
file stem matches the slug.

## Config

The config lives at `$XDG_CONFIG_HOME/hallouminate/config.toml`
(`~/.config/hallouminate/config.toml` by default). Bootstrap with
`hallouminate config init`, check with `hallouminate config validate`.

## License

MIT — see [LICENSE](LICENSE).
