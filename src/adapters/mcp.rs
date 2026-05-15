//! MCP adapter — stdio JSON-RPC transport exposing `ground`, `index`, and
//! `list_corpora` as MCP tools. All business logic stays in the domain/app
//! layers; this module is a thin shim that adapts MCP's `tools/call` shape
//! to the existing CLI command functions and back.

mod server;
mod tools;

pub use server::serve_stdio;
