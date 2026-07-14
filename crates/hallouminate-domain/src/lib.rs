//! Core domain logic, independent of any I/O framework.
//!
//! Owns the corpus model, file indexing, embeddings, hybrid search, and the
//! ground (markdown wiki) store, plus the shared types and error type in
//! [`common`].

pub mod common;
pub mod corpus;
pub mod discovery;
pub mod embeddings;
pub mod footnotes;
pub mod ground;
pub mod indexer;
pub mod repository;
pub mod search;
