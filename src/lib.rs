//! Hallouminate: a local semantic search and retrieval engine over code and
//! markdown corpora.
//!
//! The crate follows a hexagonal layout:
//!
//! - [`domain`] holds the core logic — corpora, indexing, embeddings, search,
//!   and the ground (markdown wiki) store — with no I/O framework concerns.
//! - [`adapters`] wires the domain to the outside world: the LanceDB vector
//!   store and the MCP server.
//! - [`app`] is the entry layer: CLI parsing, configuration, the daemon, and
//!   logging.

pub mod adapters;
pub mod app;
pub mod domain;
