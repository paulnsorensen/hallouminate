# Installation

## Prerequisites

- A Rust toolchain (`cargo`).
- `protoc` (the Protocol Buffers compiler) — the `lancedb` build needs it.
  - macOS: `brew install protobuf`
  - Debian/Ubuntu: `apt install protobuf-compiler`

## From crates.io

```sh
cargo install hallouminate --locked
```

The binary installs to `~/.cargo/bin/hallouminate`. Verify it:

```sh
hallouminate --version
```

## From source

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

`/install` runs `cargo install hallouminate`, registers the MCP server, then
asks where and how your first wiki should live (Socratic style) before
scaffolding, indexing, and committing it with git.
