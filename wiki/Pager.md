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

## Read Path

`src/btree.rs` implements the milestone 1 read path for foreign SQLite
files (databases written by other SQLite implementations, e.g. the SQLite
CLI or CPython `sqlite3`):

| Input | Handling |
|---|---|
| `100`-byte header | Page size (`1` means `65536`) and text encoding (`UTF-8` / `UTF-16LE` / `UTF-16BE`) |
| `Table` pages | Leaf (`0x0D`) and interior (`0x05`) pages, traversed via the cell-pointer array |
| `Leaf` cells | `rowid` plus record payload (serial types `0`-`9`, `TEXT` / `BLOB` with file encoding, `INTEGER` widths, `FLOAT`, constants `0` / `1`) |
| `Overflow` chains | Payloads larger than one page are reassembled page by page |
| `Rowid` lookup | Interior-page descent to a single `rowid` (`btree::find_rowid`) |
| `Schema` | Page 1 yields `sqlite_master`; user-table `CREATE TABLE` statements are parsed with the existing parser (`ColumnDef`), with a lenient fallback for sized types and table constraints |

`INTEGER PRIMARY KEY` columns aliasing the `rowid` are filled from the
cell `rowid` when the record stores `NULL`; short records are padded with
column defaults (SQLite `ALTER TABLE` semantics). Index pages (`0x02` /
`0x0A`) carry no table rows and are skipped during scans. `sqlite_%`
internal tables are skipped. `WITHOUT ROWID` tables are rejected with
`Err(SqlError::Unsupported)`. Files larger than 256 MiB are refused with
`Err(SqlError::Unsupported)` as a denial-of-service guard.

```rust
let conn = Connection::open("/tmp/other.sqlite").unwrap();
let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", (), |row| row.get(0)).unwrap();
```

## Foreign-File Policy

`Connection::open` validates the 100-byte header first:

- Missing magic returns `Err(SqlError::NotSqliteFile)`.
- Valid magic with the `TSQL01` marker loads the snapshot below.
- Valid magic without the marker is read as a foreign SQLite B-Tree file
  (read path above): all user-table rows are loaded into the in-memory
  engine.

> **Note:** Foreign files are read-only input. The first write persists the
> in-memory tables in the snapshot format, replacing the foreign layout.
> Native B-Tree writes are milestone 2 work.

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
