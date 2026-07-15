# Installation

The fastest path is the prebuilt-binary installer — no Rust toolchain, no
`protoc`, no compile. Build from source with cargo only if your platform has no
prebuilt, or you want a development checkout.

## Prebuilt binary (recommended)

Downloads a prebuilt `hallouminate` for your platform and adds it to your PATH:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/paulnsorensen/hallouminate/releases/latest/download/hallouminate-installer.sh | sh
```

Verify it:

```sh
hallouminate --version
```

Prebuilt binaries are published for each release:

| Platform | Target |
| --- | --- |
| macOS, Apple Silicon | `aarch64-apple-darwin` |
| Linux, arm64 | `aarch64-unknown-linux-gnu` (glibc ≥ 2.39) |
| Linux, x86-64 | `x86_64-unknown-linux-gnu` (glibc ≥ 2.39) |

Re-run the one-liner any time to upgrade to the latest release. On Intel Mac,
Windows, or an older glibc, build from source with cargo below.

## From crates.io

Builds from source, so it works on any platform with a Rust toolchain — at the
cost of compiling native dependencies (a few minutes).

### Prerequisites

- A Rust toolchain (`cargo`) — see <https://rustup.rs>.
- `protoc` (the Protocol Buffers compiler) — the `lancedb` build needs it.
  - macOS: `brew install protobuf`
  - Debian/Ubuntu: `apt install protobuf-compiler`

```sh
cargo install hallouminate --locked
```

The binary installs to `~/.cargo/bin/hallouminate` (make sure that's on your
PATH).

## From source

Same prerequisites as crates.io. Clone and build:

```sh
git clone https://github.com/paulnsorensen/hallouminate.git
cd hallouminate
cargo build --release
```

The binary lands in `target/release/hallouminate`.

## Install for your harness

Every integration launches the same `hallouminate serve` stdio server. The
plugin routes also install the bundled skills:

| Harness | Install the plugin / skills | MCP registration |
| --- | --- | --- |
| **Claude Code** | `/plugin marketplace add paulnsorensen/hallouminate` → `/plugin install hallouminate@hallouminate` | Bundled `.mcp.json`; user fallback: `claude mcp add hallouminate --scope user -- hallouminate serve` |
| **Codex** | `codex plugin marketplace add paulnsorensen/hallouminate`, restart, then `codex plugin add hallouminate@hallouminate` (or install from `/plugins`) | Bundled `.mcp.json` |
| **Copilot CLI** | `copilot plugin marketplace add paulnsorensen/hallouminate` → `copilot plugin install hallouminate@hallouminate` | Bundled `.mcp.json` |
| **OMP** | `/marketplace add paulnsorensen/hallouminate` → `/marketplace install hallouminate@hallouminate` | Bundled Claude-compatible `.mcp.json` |
| **Cursor** | Teams/Enterprise: import `https://github.com/paulnsorensen/hallouminate` under **Plugins → Team Marketplaces**. Local: clone, copy or symlink `plugins/hallouminate` to `~/.cursor/plugins/local/hallouminate`, then reload/restart Cursor. | Bundled `.mcp.json` through the Cursor manifest |
| **Gemini CLI** | From a checkout: `gemini extensions install ./plugins/hallouminate --consent`. From an extracted release archive: `gemini extensions install ./hallouminate-skills-<version>/plugins/hallouminate --consent`. | Inline in `gemini-extension.json`; bundled skills are auto-discovered |
| **opencode** | Copy `plugins/hallouminate/skills/` to `~/.config/opencode/skills/` | Add `{ "mcp": { "hallouminate": { "type": "local", "command": ["hallouminate", "serve"] } } }` to `opencode.json` |

`hallouminate serve` auto-spawns the daemon if none is running, so there is no
separate process to manage.

## Bootstrap a config

```sh
hallouminate config init       # scaffold the XDG baseline config
hallouminate config validate   # confirm it parses
```

See [Configuration](./config.md) for what goes in the config and how the
XDG baseline merges with a repo-layer `.hallouminate/config.toml`.
