# Pager

File format, atomic persistence, and the foreign-file policy.

## File Layout

| Offset | Length | Content |
|---|---|---|
| `0` | `16` | `SQLite format 3\0` magic |
| `16` | `2` | Page size, big-endian (`4096`) |
| `18` | `82` | Standard SQLite header fields |
| `100` | `6` | `TSQL01` snapshot marker |
| `106` | `4` | Snapshot version, big-endian (`1`) |
| `110` | `*` | JSON snapshot of tables |

Every file SQLKit creates is recognized as SQLite by magic-based detectors. The JSON snapshot is an interim payload until the B-Tree pager subagent implements native page read and write.

## Atomic Snapshots

Saves serialize tables to `<name>.tsql-tmp`, call `fsync`, then rename over the target. Readers never observe a half-written database. New files receive restrictive permissions on Unix. Query results stream through `MappedRows`, so only one row is materialized at a time regardless of file size.

```rust
let conn = Connection::open("/tmp/app.sqlite").unwrap();
conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
conn.close().unwrap();
```

## Foreign-File Policy

`Connection::open` validates the 100-byte header first:

- Missing magic returns `Err(SqlError::NotSqliteFile)`.
- Valid magic without the `TSQL01` marker returns `Err(SqlError::Unsupported)` instead of misreading foreign B-Tree pages.

> **Note:** Full interop with SQLite files written by rusqlite or the SQLite CLI is roadmap work for the pager subagent, not part of the basis.

## Usage / Example

```rust
use sqlkit::Connection;

let dir = tempfile::tempdir().unwrap();
let path = dir.path().join("app.sqlite");
Connection::open(&path).unwrap().close().unwrap();
let raw = std::fs::read(&path).unwrap();
assert!(raw.starts_with(b"SQLite format 3\0"));
```

## Cross References

- [Connection.md](Connection.md) – open, checkpoint, and close behavior
- [Statement.md](Statement.md) – streaming reads over paged data
