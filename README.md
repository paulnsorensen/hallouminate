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

`hallouminate serve` starts a stdio MCP server. It exposes semantic grounding,
manual indexing, corpus discovery, file listing, and `add_markdown` for writing
plain markdown into a configured corpus. Markdown content is stored verbatim;
callers own the directory structure and document format. After `add_markdown`
writes a file, the server refreshes that corpus' LanceDB index automatically.

## License

MIT — see [LICENSE](LICENSE).
