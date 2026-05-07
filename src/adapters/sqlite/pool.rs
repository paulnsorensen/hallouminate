use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;

use crate::domains::common::Result;

const MEMORY_DB: &str = ":memory:";

fn ensure_sqlite_vec_registered() {
    static VEC_INIT: Once = Once::new();
    VEC_INIT.call_once(|| unsafe {
        // SAFETY: ported from tern-vectors/src/config.rs. `sqlite3_vec_init`
        // has the C `sqlite3_x_init` extension ABI, but the Rust binding is
        // exposed as `fn()`; transmute is the only way to reinterpret the
        // signature for `sqlite3_auto_extension`.
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

pub fn open_db(path: &Path) -> Result<Connection> {
    ensure_sqlite_vec_registered();
    let conn = if path == Path::new(MEMORY_DB) {
        Connection::open_in_memory()?
    } else {
        Connection::open(path)?
    };
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_loads_sqlite_vec() {
        let conn = open_db(Path::new(MEMORY_DB)).expect("open :memory: db");
        let version: String = conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version() should be available after auto_extension registration");
        assert!(
            !version.is_empty(),
            "vec_version() returned empty string: {version}"
        );
    }

    #[test]
    fn open_in_memory_twice_is_safe() {
        let _first = open_db(Path::new(MEMORY_DB)).expect("first open");
        let second = open_db(Path::new(MEMORY_DB)).expect("second open should reuse Once");
        let version: String = second
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version() on second connection");
        assert!(!version.is_empty());
    }
}
