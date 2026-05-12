mod pool;
mod queries;
mod schema;

use crate::domain::common::HallouminateError;

pub use pool::{open_db, DbConn};
pub use queries::chunks::{delete_chunks_for_file, insert_chunk, NewChunk};
pub use queries::vec::{insert_vec, knn_chunks, EMBEDDING_DIM};
pub use queries::{
    all_files_for_corpus, delete_file_cascade, get_file_by_ref, touch_mtime, upsert_file, FileRow,
    NewFile,
};
pub use schema::apply_schema;

impl From<rusqlite::Error> for HallouminateError {
    fn from(err: rusqlite::Error) -> Self {
        HallouminateError::Db(Box::new(err))
    }
}
