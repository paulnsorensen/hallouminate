# 🧀 hallouminate 🧀

[![CI](https://img.shields.io/github/actions/workflow/status/paulnsorensen/hallouminate/ci.yml?branch=main&label=CI&style=flat-square)](https://github.com/paulnsorensen/hallouminate/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hallouminate?style=flat-square)](https://crates.io/crates/hallouminate)
[![License: MIT](https://img.shields.io/github/license/paulnsorensen/hallouminate?style=flat-square)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/paulnsorensen/hallouminate?style=flat-square)](https://github.com/paulnsorensen/hallouminate/releases/latest)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/paulnsorensen/hallouminate/badge)](https://scorecard.dev/viewer/?uri=github.com/paulnsorensen/hallouminate)
[![Conventional Commits](https://img.shields.io/badge/Conventional%20Commits-1.0.0-yellow?style=flat-square)](https://www.conventionalcommits.org)
[![Agent Skills](https://img.shields.io/badge/Agent%20Skills-spec-blueviolet?style=flat-square)](https://agentskills.io/specification)
[![PRs welcome](https://img.shields.io/badge/PRs-welcome-brightgreen?style=flat-square)](https://github.com/paulnsorensen/hallouminate/pulls)
[![Buy Me a Coffee](https://img.shields.io/badge/Buy_Me_a_Coffee-FFDD00?style=flat-square&logo=buymeacoffee&logoColor=black)](https://www.buymeacoffee.com/paulnsorensen)

**Stop hallucinating. Start hallouminating.**

> _"The wiki must flow."_

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

## Install

The default install is a prebuilt binary — no Rust toolchain, no `protoc`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/paulnsorensen/hallouminate/releases/latest/download/hallouminate-installer.sh | sh
```

Prebuilts cover Apple-silicon macOS (`aarch64-apple-darwin`) and x86_64 /
aarch64 Linux. Alternatives, in cascade order:

```sh
cargo binstall hallouminate          # same prebuilts, via dist-manifest.json
cargo install hallouminate --locked  # source build — needs Rust + protoc
```

Intel macOS has no prebuilt (`ort`/ONNX Runtime ships none — pykeio/ort#556);
use the source build there. **Windows is unsupported** — the daemon is
Unix-only (Unix domain socket + `flock`); see
[#48](https://github.com/paulnsorensen/hallouminate/issues/48).

Verify with `hallouminate --version`.

### Per-harness setup

The MCP server is always the same command: `hallouminate serve` (stdio).
What differs per harness is how it gets registered:

| Harness | Plugin / skills | MCP registration |
| --- | --- | --- |
| **Claude Code** | `/plugin marketplace add paulnsorensen/hallouminate` → `/plugin install hallouminate@hallouminate` → run `/hallouminate:install` | Declarative — the plugin bundles `.mcp.json` (project scope). User scope fallback: `claude mcp add hallouminate --scope user -- hallouminate serve` |
| **Codex** | `codex plugin marketplace add paulnsorensen/hallouminate`, restart, install from the `/plugins` directory | Bundled `.mcp.json` in the plugin payload |
| **opencode** | Copy the skills: `cp -r plugins/hallouminate/skills/ .agents/skills/` (or `~/.config/opencode/skills/`) | Add to `opencode.json`: `{ "mcp": { "hallouminate": { "type": "local", "command": ["hallouminate", "serve"] } } }` |
| **Copilot CLI** | — (binary + MCP only) | Add to `~/.copilot/mcp-config.json`: `{ "mcpServers": { "hallouminate": { "command": "hallouminate", "args": ["serve"] } } }` |
| **Cursor** | — (binary + MCP only) | Add to `~/.cursor/mcp.json`: `{ "mcpServers": { "hallouminate": { "command": "hallouminate", "args": ["serve"] } } }` |

### First run

1. `hallouminate config init` — scaffold the XDG baseline config.
2. `hallouminate init-repo <name>` in your repo — seed
   `.hallouminate/config.toml` plus the wiki skeleton; the wiki becomes the
   `repo:<name>:wiki` corpus. Identical on every harness.
3. `hallouminate index` — build the index (auto-spawns the daemon and
   downloads the embedding model on first use).
4. `hallouminate ground "<a question your wiki answers>"` — prove the loop.

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

## How the daemon works

A long-lived local daemon owns the LanceDB ground directory, the repository
registry, and per-corpus mutation locks. The CLI and the stdio MCP server are
thin clients that talk to it over a Unix domain socket.

- **Auto-spawn** — `hallouminate serve`, `index`, and `ground` start a
  detached daemon automatically when none is listening; there is nothing to
  start by hand.
- **Socket resolution order** — `HALLOUMINATE_SOCKET` (explicit full-path
  override; setting it also disables auto-spawn — the caller owns the daemon
  lifecycle), else `$XDG_RUNTIME_DIR/hallouminate/daemon.sock`, else
  `~/.cache/hallouminate/daemon.sock`.
- **Lifecycle** — `hallouminate daemon status` / `stop` / `restart`; bare
  `hallouminate daemon` runs it in the foreground. Only one instance per
  socket can run (`flock`-guarded).
- **Version-skew respawn** — after a binary upgrade, the next client pings
  the running daemon and compares versions; a mismatch stops the stale daemon
  and spawns a fresh one. No manual restart needed after upgrades.
- **Diagnostics** — anything the auto-spawned daemon emits before its logger
  is up (panics, early config errors) lands in
  `~/.local/state/hallouminate/daemon-bootstrap.log` (`$XDG_STATE_HOME`).
- **Windows** — the daemon model is Unix-only; see
  [#48](https://github.com/paulnsorensen/hallouminate/issues/48).

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

A cross-harness plugin pack ships in this repo under
[`plugins/hallouminate`](plugins/hallouminate): skills for installing
hallouminate and authoring wikis, plus a bundled `.mcp.json` that registers
the MCP server declaratively. Claude Code installs it from
`.claude-plugin/marketplace.json`, Codex from `.agents/plugins/marketplace.json`
— see the [install matrix](#per-harness-setup). `tests/plugin_manifests.rs`
pins the pack's manifests to the crate version, and the `release-skills`
workflow publishes versioned skill-pack archives to GitHub Releases on every
`v*` release tag.

## License

MIT — see [LICENSE](LICENSE).
