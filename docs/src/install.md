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

## Register the MCP server

Point your agent at `hallouminate serve` — the stdio MCP server that exposes
the wiki tools. With Claude Code:

```sh
claude mcp add hallouminate -- hallouminate serve
```

`hallouminate serve` auto-spawns the daemon if none is running, so there's no
separate process to manage.

## Bootstrap a config

```sh
hallouminate config init       # scaffold the XDG baseline config
hallouminate config validate   # confirm it parses
```

See [Configuration](./config.md) for what goes in the config and how the
XDG baseline merges with a repo-layer `.hallouminate/config.toml`.

## Claude Code skill pack

A Claude Code plugin ships in this repo under
[`plugins/hallouminate`](https://github.com/paulnsorensen/hallouminate/tree/main/plugins/hallouminate).
It installs hallouminate and bootstraps your first wiki interactively:

```text
/plugin marketplace add paulnsorensen/hallouminate
/plugin install hallouminate@hallouminate
/hallouminate:install
```

`/install` installs the binary, registers the MCP server, then asks where and
how your first wiki should live (Socratic style) before scaffolding, indexing,
and committing it with git.
