# hallouminate

A markdown corpus indexer for LLMs to build and query their own per-repo
wikis. Hallouminate stores markdown verbatim on disk, embeds it with
[fastembed](https://github.com/Anush008/fastembed-rs), indexes the embeddings
in [LanceDB](https://lancedb.com/), and exposes a small MCP surface so an LLM
can author and search a per-repo knowledge base without leaving its agent loop.

The filesystem is the source of truth; LanceDB rows are derived and refreshed
automatically when an LLM writes via `add_markdown`, or in bulk via
`hallouminate index`. Code files (`.rs`, `.toml`, …) can also be indexed as
text for semantic search, but hallouminate does no structural analysis — it's
a wiki indexer that happens to tolerate code, not a code-intelligence tool.

## Why a daemon

A long-lived local daemon owns the LanceDB ground directory, per-corpus
mutation locks, and config resolution. The CLI and the stdio MCP server both
talk to it over a Unix domain socket — one owner, no cross-process LanceDB
races. See [Architecture](./architecture.md) for the full picture.

## Where to go next

- **[Installation](./install.md)** — install the binary and register the MCP
  server with your agent.
- **[CLI reference](./cli.md)** — every subcommand and its flags.
- **[MCP surface](./mcp.md)** — the nine tools an LLM calls to author and
  search wikis.
- **[Configuration](./config.md)** — the XDG baseline, repo-layer merge, and
  embedding-model options.
- **[Dogfooding](./dogfooding.md)** — this repo maintains its own wiki with
  hallouminate; here's how to read it.

## License

MIT — see [LICENSE](https://github.com/paulnsorensen/hallouminate/blob/main/LICENSE).
