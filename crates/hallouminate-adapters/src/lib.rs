//! Adapters connecting the domain to external systems: the LanceDB vector
//! store and FastEmbed model backends.

mod crossencoder;
mod embedder;
mod lance;

pub use crossencoder::FastembedCrossencoder;
pub use embedder::{EMBEDDING_DIM, EmbedBatch, EmbedRole, Embedder, instruction_prefix};
pub use lance::{
    CorpusChunkStats, LanceStore, MaintenanceOptions, MaintenanceStats, chunk_id_for,
    chunks_schema, default_schema_version_pub,
};
