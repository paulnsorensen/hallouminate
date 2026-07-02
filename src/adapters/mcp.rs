//! MCP adapter — stdio JSON-RPC transport exposing the ten hallouminate MCP
//! tools (`ground`, `index`, `list_tree`, `list_files`, `add_markdown`,
//! `read_markdown`, `delete_markdown`, `corpus_stats`, `list_corpora`,
//! `get_footnote`). All business logic stays in the domain/app layers; this
//! module is a thin shim that adapts MCP's `tools/call` shape to the existing
//! CLI command functions and back.

mod server;
mod tools;

pub use server::serve_stdio;
