//! Adapters connecting the domain to external systems: the LanceDB vector
//! store ([`lance`]) and the MCP server ([`mcp`]).

pub mod crossencoder;
pub mod embedder;
pub mod lance;
pub mod mcp;
