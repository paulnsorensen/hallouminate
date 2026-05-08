use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;

use crate::domains::common::Result;

const MEMORY_DB: &str = ":memory:";

/// Opaque SQLite connection handle. Hides the rusqlite binding from callers
/// outside the slice; siblings inside the slice reach the wrapped
/// connection via `raw()`.
pub struct DbConn(Connection);

impl DbConn {
    pub(crate) fn raw(&self) -> &Connection {
        &self.0
    }
}

fn ensure_sqlite_vec_registered() {
    static VEC_INIT: Once = Once::new();
    VEC_INIT.call_once(|| unsafe {
        // SAFETY: `sqlite3_vec_init` is the canonical sqlite3 extension
        // entry point with the
        // `(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int`
        // ABI. The Rust binding exposes it as a bare `fn()` for FFI import
        // convenience, so we transmute the function pointer back to its
        // real signature. The `Once` guard ensures single registration;
        // the underlying C function is reentrant-safe per the sqlite3
        // extension contract.
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::ffi::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
        if rc != 0 {
            tracing::error!("sqlite-vec auto_extension registration failed: rc={rc}");
        }
    });
}

pub fn open_db(path: &Path) -> Result<DbConn> {
    ensure_sqlite_vec_registered();
    let conn = if path == Path::new(MEMORY_DB) {
        Connection::open_in_memory()?
    } else {
        Connection::open(path)?
    };
    // foreign_keys is per-connection and OFF by default. Enforce here so
    // every connection gets cascade behaviour, regardless of whether the
    // caller bootstraps the schema.
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    Ok(DbConn(conn))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_vec_version_format(version: &str) {
        assert!(
            version.starts_with('v'),
            "vec_version() should start with 'v', got: {version}"
        );
        assert!(
            version.split('.').count() >= 2,
            "vec_version() should be dotted (e.g. v0.1.6), got: {version}"
        );
    }

    #[test]
    fn open_in_memory_loads_sqlite_vec() {
        let db = open_db(Path::new(MEMORY_DB)).expect("open :memory: db");
        let version: String = db
            .raw()
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version() should be available after auto_extension registration");
        assert_vec_version_format(&version);
    }

    #[test]
    fn open_in_memory_twice_is_safe() {
        let _first = open_db(Path::new(MEMORY_DB)).expect("first open");
        let second = open_db(Path::new(MEMORY_DB)).expect("second open should reuse Once");
        let version: String = second
            .raw()
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version() on second connection");
        assert_vec_version_format(&version);
    }

    #[test]
    fn open_db_enforces_foreign_keys() {
        let db = open_db(Path::new(MEMORY_DB)).expect("open");
        let on: i64 = db
            .raw()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("query pragma");
        assert_eq!(on, 1, "open_db must set PRAGMA foreign_keys = ON");
    }
}
