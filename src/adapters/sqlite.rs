mod pool;
mod queries;
mod schema;

use crate::domain::common::HallouminateError;

pub use pool::{DbConn, open_db};
pub use queries::chunks::{NewChunk, delete_chunks_for_file, insert_chunk};
pub use queries::vec::{EMBEDDING_DIM, delete_vec_for_chunk, insert_vec, knn_chunks};
pub use queries::{
    FileRow, NewFile, all_files_for_corpus, delete_file_cascade, get_file_by_ref, touch_mtime,
    upsert_file,
};
pub use schema::apply_schema;

impl From<rusqlite::Error> for HallouminateError {
    fn from(err: rusqlite::Error) -> Self {
        HallouminateError::Db(Box::new(err))
    }
}
