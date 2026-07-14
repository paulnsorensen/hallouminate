//! Single integration-test harness binary. One target relinks once per
//! domain edit instead of the 12 formerly-separate `tests/*.rs` binaries.

mod common;

mod cli_ground;
mod cli_index;
mod cross_repo_union;
mod daemon;
mod fixture_e2e;
mod lancedb_integration;
mod mcp_serve;
mod model_mismatch;
mod multi_format;
mod plugin_manifests;
mod real_tokenizer;
mod recovery;
