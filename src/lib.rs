//! Tontoo SQLKit – dependency-light SQLite-compatible engine for TontooOS.
//!
//! SQLKit exposes a `rusqlite`-compatible subset (`Connection`, `Statement`,
//! `params!`, `Row`, `Transaction`) over a pure-Rust engine with a SQLite
//! file header, atomic snapshots, and streaming iterators.
//!
//! Basis scope (this commit): `CREATE TABLE` / `CREATE INDEX`, `INSERT`
//! (with `OR REPLACE` / `OR IGNORE`), `SELECT` with `WHERE col = ?` filters,
//! `UPDATE`, `DELETE`, `PRAGMA`, transactions, and file persistence.
//! JOIN, sub-selects, triggers, views, and the full B-Tree page layout are
//! roadmap items for follow-up subagents (see `wiki/MAIN.md`).
//!
//! # Quick Start
//! ```rust
//! use sqlkit::{params, Connection};
//!
//! let conn = Connection::open_in_memory().unwrap();
//! conn.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT)").unwrap();
//! conn.execute("INSERT INTO notes (id, title) VALUES (?1, ?2)", params!["a", "Hello"]).unwrap();
//! let title: String = conn.query_row("SELECT title FROM notes WHERE id = ?1", params!["a"], |row| row.get(0)).unwrap();
//! assert_eq!(title, "Hello");
//! ```

pub mod connection;
pub mod error;
pub mod ffi;
pub mod pager;
pub mod parser;
pub mod statement;
pub mod transaction;
pub mod value;

pub use connection::{Column, Connection, Table};
pub use error::{Result, SqlError};
pub use statement::{ColumnIndex, MappedRows, Row, Statement};
pub use transaction::Transaction;
pub use value::{FromValue, IntoParams, ToSql, Value};

#[cfg(target_os = "windows")]
compile_error!("SQLKit only supports TontooOS / Arch Linux – use WSL ArchLinux (wsl -d archlinux)");

/// Library version: (major, minor, patch).
pub const SQLKIT_VERSION: (u32, u32, u32) = (26, 1, 0);
/// Library version string.
pub const SQLKIT_VERSION_STR: &str = "26.1.0";

/// Convenience prelude mirroring the ruslite items CoreData uses.
pub mod prelude {
    pub use crate::connection::Connection;
    pub use crate::error::{Result, SqlError};
    pub use crate::statement::{Row, Statement};
    pub use crate::transaction::Transaction;
    pub use crate::value::{FromValue, IntoParams, ToSql, Value};
    pub use crate::{params, SQLKIT_VERSION, SQLKIT_VERSION_STR};
}

#[cfg(test)]
mod integration_tests {
    use crate::{params, Connection};

    fn coredata_schema(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS objects (
                id TEXT PRIMARY KEY,
                entity TEXT NOT NULL,
                data BLOB NOT NULL,
                rev INTEGER NOT NULL,
                updated_at TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_entity ON objects(entity);
             CREATE INDEX IF NOT EXISTS idx_deleted ON objects(deleted);",
        )
        .unwrap();
    }

    #[test]
    fn coredata_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        coredata_schema(&conn);
        let blob = vec![1u8, 2u8, 3u8];
        conn.execute(
            "INSERT OR REPLACE INTO objects (id, entity, data, rev, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!["id-1", "Note", blob.clone(), 1i32, "2026-01-01", 0i32],
        )
        .unwrap();
        let back: Vec<u8> = conn
            .query_row("SELECT data FROM objects WHERE id = ?1", params!["id-1"], |row| row.get(0))
            .unwrap();
        assert_eq!(back, blob);
        let rows: Vec<Vec<u8>> = conn
            .prepare("SELECT data FROM objects WHERE entity = ?1")
            .unwrap()
            .query_map(params!["Note"], |row| row.get(0))
            .unwrap()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn file_roundtrip_keeps_sqlite_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            coredata_schema(&conn);
            conn.execute(
                "INSERT INTO objects (id, entity, data, rev, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["x", "Note", vec![9u8], 1i32, "t", 0i32],
            )
            .unwrap();
            conn.close().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(b"SQLite format 3\0"));
        let conn = Connection::open(&path).unwrap();
        let count: String = conn
            .query_row("SELECT id FROM objects WHERE id = ?1", params!["x"], |row| row.get(0))
            .unwrap();
        assert_eq!(count, "x");
    }
}
