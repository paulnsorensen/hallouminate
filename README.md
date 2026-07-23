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
aarch64 Linux (glibc ≥ 2.39). Re-run the one-liner any time to upgrade.
Alternatives, in cascade order:

```sh
npx hallouminate --help              # npm shim — postinstall downloads the same prebuilt
cargo binstall hallouminate          # same prebuilts, via dist-manifest.json
cargo install hallouminate --locked  # source build — needs Rust + protoc
```

The npm package is a thin shim: `postinstall` fetches the matching prebuilt
for your platform from the GitHub release and verifies its SHA-256.

Source builds need `protoc` (the `lancedb` build dependency:
`brew install protobuf` / `apt install protobuf-compiler`); from a git
checkout, `cargo build --release` lands the binary at
`target/release/hallouminate`.

Intel macOS and older-glibc Linux have no prebuilt (`ort`/ONNX Runtime ships
no Intel-mac build — pykeio/ort#556); use the source build there. **Windows
is unsupported** — the daemon is Unix-only (Unix domain socket + `flock`);
see [#48](https://github.com/paulnsorensen/hallouminate/issues/48).

Verify with `hallouminate --version`.

### Per-harness setup

The MCP server is always `hallouminate serve` over stdio. Install the shared
plugin pack through the harness-native route:

| Harness | Install the plugin / skills | MCP registration |
| --- | --- | --- |
| **Claude Code** | `/plugin marketplace add paulnsorensen/hallouminate` → `/plugin install hallouminate@hallouminate` | Bundled `.mcp.json`; user fallback: `claude mcp add hallouminate --scope user -- hallouminate serve` |
| **Codex** | `codex plugin marketplace add paulnsorensen/hallouminate`, restart, then `codex plugin add hallouminate@hallouminate` (or install from `/plugins`) | Bundled `.mcp.json` |
| **Copilot CLI** | `copilot plugin marketplace add paulnsorensen/hallouminate` → `copilot plugin install hallouminate@hallouminate` | Bundled `.mcp.json` |
| **OMP** | `/marketplace add paulnsorensen/hallouminate` → `/marketplace install hallouminate@hallouminate` | Bundled Claude-compatible `.mcp.json` |
| **Cursor** | Teams/Enterprise: import `https://github.com/paulnsorensen/hallouminate` under **Plugins → Team Marketplaces**. Local: clone, copy or symlink `plugins/hallouminate` to `~/.cursor/plugins/local/hallouminate`, then reload/restart Cursor. | Bundled `.mcp.json` through the Cursor manifest |
| **Gemini CLI** | From a checkout: `gemini extensions install ./plugins/hallouminate --consent`. From an extracted release archive: `gemini extensions install ./hallouminate-skills-<version>/plugins/hallouminate --consent`. | Inline in `gemini-extension.json`; bundled skills are auto-discovered |
| **opencode** | Copy `plugins/hallouminate/skills/` to `~/.config/opencode/skills/` | Add `{ "mcp": { "hallouminate": { "type": "local", "command": ["hallouminate", "serve"] } } }` to `opencode.json` |

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

## MCP

`hallouminate serve` starts a stdio MCP server. Tools:

- `ground` — semantic search.
- `index` — bulk (re)build a corpus index.
- `corpus_stats` — index health for one corpus: indexed file count, total
  chunk rows, newest index timestamp, and unindexed-file count.
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
- `get_footnote` — resolve a single citation: the footnote target for a
  page's `#footnote_number`.

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

## Cross-repo union search

`ground` (and the read/list tools) resolve corpora relative to the caller's
working directory:

- **Inside a repo** — the request defaults to that repo's `repo:<name>:wiki`.
- **Above all repos** (e.g. `cd ~/Dev`) — a `ground` call with **no explicit
  `corpus`** searches the _union_ of every effective corpus: discovered sub-repo
  wikis + baseline-registered `[[repository]]` wikis, plus user-declared
  `[[corpus]]` entries and each repository's `repo:<name>:corpus` source corpus
  when configured. The results are merged and re-ranked into one response, and
  **each hit is attributed to its source corpus** (file-level `corpus` plus
  per-chunk `provenance.corpus`).

The downward walk is bounded: it honours `.gitignore`, skips hidden
directories (except `.hallouminate` itself), caps its depth, and never scans
above the working directory. Walk-discovered wikis are deduped against the
baseline by resolved path; a discovered local config that collides with a
baseline repository of the same name wins, with a `cross-repo-union` warning
on the response rather than a silent shadow.

Passing an explicit `corpus` always pins the search to that one corpus,
unchanged. **Writes** (`add_markdown` / `delete_markdown`) still require an
explicit single-root corpus — the multi-root union is read- and search-only.

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
- **Process logs** — `$XDG_STATE_HOME/hallouminate/hallouminate.log`
  (default `~/.local/state/hallouminate/hallouminate.log`) rotates exactly at
  10 MiB into numbered archives and retains at most 100 MiB. Configure
  `[logging].max_file_bytes` / `max_total_bytes`, or override them with
  `HALLOUMINATE_LOG_MAX_FILE_BYTES` / `HALLOUMINATE_LOG_MAX_TOTAL_BYTES`.
  `[watch].failure_reminder_secs` defaults to 60 seconds; override it with
  `HALLOUMINATE_WATCH_FAILURE_REMINDER_SECS` or set `0` to disable suppression.
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
hallouminate and authoring wikis, plus MCP registration for each supported
plugin format. Claude Code and OMP use `.claude-plugin/marketplace.json`,
Codex uses `.agents/plugins/marketplace.json`, Copilot CLI uses the payload's
root `plugin.json`, Cursor uses `.cursor-plugin/`, and Gemini CLI uses
`gemini-extension.json` — see the [install matrix](#per-harness-setup).
`tests/plugin_manifests.rs` pins every manifest to the crate version, and the
`release-skills` workflow publishes versioned plugin-pack archives on every
`v*` release tag.

## License

MIT — see [LICENSE](LICENSE).
