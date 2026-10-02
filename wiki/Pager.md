# Pager

File format, native B-Tree writes, crash safety, and the foreign-file policy.

## File Layout

Every file SQLKit creates is a byte-level real SQLite database (page size
`4096`, UTF-8 encoding), readable by rusqlite, the SQLite CLI, and CPython
`sqlite3` — and files written by them stay readable through the milestone 1
read path below.

| Offset | Length | Content |
|---|---|---|
| `0` | `16` | `SQLite format 3\0` magic |
| `16` | `2` | Page size, big-endian (`4096`) |
| `18` | `82` | Standard SQLite header fields (change counter, page count, freelist trunk, schema cookie, encoding `1`) |
| `100` | `*` | Page 1: `sqlite_master` table B-Tree root (leaf `0x0D`, or interior `0x05` for huge schemas) |
| `4096*n` | `4096` | Table leaves (`0x0D`) and interiors (`0x05`), index leaves (`0x0A`) and interiors (`0x02`), overflow pages, freelist trunk pages |

```rust
let conn = Connection::open("/tmp/app.sqlite").unwrap();
conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
conn.close().unwrap();
```

## Write Path

`src/btree_write.rs` rebuilds the whole database image from the in-memory
tables on every commit and replaces the file atomically (temp file in the
same directory + `fsync` + rename + directory `fsync`).

| Step | Handling |
|---|---|
| `Record` | Serial types `0`-`9` plus `TEXT` (`13 + 2 * len`) / `BLOB` (`12 + 2 * len`) in UTF-8; `INTEGER` uses minimal widths (`0`/`1` as constants `8`/`9`); `INTEGER PRIMARY KEY` aliases are stored as `NULL` with the value in the cell `rowid` |
| `Rowid` | `max + 1` per table; explicit `INTEGER PRIMARY KEY` values double as rowids; `NULL` aliases auto-assign (and are filled into the row, SQLite style) |
| `Overflow` | Table leaves spill with the standard formula (`K = M + ((P - M) % (U - 4))`, shared with the reader); index pages use the index `((U-12)*64/255)-23` variant |
| `Split` | Leaf cells pack greedily in key order; interior levels derive bottom-up with divider keys (max `rowid` / max index key); root splits promote a new interior root |
| `Master` | `sqlite_master` rows for every table and plain index plus a schema-cookie bump on schema changes only |
| `Freelist` | Rebuilds preserve the previous file size: surplus pages link into the freelist trunk (`first trunk`, `total`, trunk chain) instead of truncating |

```rust
let conn = Connection::open("/tmp/app.sqlite").unwrap();
conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params![1i32, "hello"]).unwrap();
```

## Atomic Commits

Commits use rollback-journal mode with strict `fsync` ordering:

1. Write `<db>-journal` (SQLKit format: magic `SQLKITJ1`, version, previous file bytes) and `fsync` it plus the directory.
2. Write the new image to `<db>.sqlkit-tmp` and `fsync` it.
3. Rename over the target, `fsync` the file and the directory.
4. Delete the journal and `fsync` the directory again.

`Connection::open` recovers a leftover journal (rolls the previous bytes
back) before reading. The `PRAGMA integrity_check` of every committed file
returns `ok`. Tests simulate a crash mid-write with the
`btree_write::set_crash_point_for` hook (test-only, path-scoped).

> **Note:** The journal file is SQLKit-format, not SQLite-format. A foreign
> (real SQLite) journal next to the file is refused with
> `Err(SqlError::Unsupported)` instead of being applied.

## WAL Policy

SQLKit is a rollback-journal engine and never replays write-ahead logs. If
a `-wal` file with content exists next to the database on open, the open
fails with `Err(SqlError::Unsupported)` instead of silently reading stale
rows. Checkpoint the file with SQLite first, then open it with SQLKit.

## Snapshot Migration

Legacy `TSQL01` snapshot files (header + marker + JSON) stay readable
exactly as before. The first write migrates them to the native layout
through the same atomic rename in the same directory; no temp files survive
a commit. Snapshot files never stored indexes, so file-side indexes appear
only for indexes created after migration.

## Snapshot JSON Codec

`src/json.rs` encodes and decodes the legacy snapshot body. It is
hand-written on top of `foundation::serialization::JsonValue`, so SQLKit
depends on Foundation only and pulls in no serialization crates.

```rust
pub fn parse_tables(bytes: &[u8]) -> Result<HashMap<String, Table>>
pub fn write_tables(tables: &HashMap<String, Table>) -> String
pub fn value_to_json(value: &Value) -> JsonValue
pub fn value_from_json(json: &JsonValue) -> Result<Value>
pub fn column_to_json(column: &Column) -> JsonValue
pub fn column_from_json(json: &JsonValue) -> Result<Column>
pub fn table_to_json(table: &Table) -> JsonValue
pub fn table_from_json(json: &JsonValue) -> Result<Table>
```

The wire format is unchanged from the previous `serde_json` backend, so
existing snapshot files keep loading:

| `Value` | JSON |
|---|---|
| `Null` | `null` |
| `Integer(i64)` | integer literal |
| `Real(f64)` | float literal, `2.0` keeps its fraction |
| `Text(String)` | JSON string |
| `Blob(Vec<u8>)` | array of byte integers, e.g. `[0,1,255]` |

| Struct | JSON members (declaration order) |
|---|---|
| `Column` | `name`, `coltype`, `primary_key`, `not_null`, `default` |
| `Table` | `name`, `columns`, `rows` |
| `HashMap<String, Table>` | one member per table name |

Behavior:

- `Column::default` may be missing in the file; it decodes as `None`. When
  present it is written as an explicit `null`, never omitted.
- Returns `Err(SqlError::Serde)` when the root is not an object, a `rows`
  entry is not an array, a `BLOB` holds a non-integer or an out-of-range
  value, a `REAL`/`TEXT` member has the wrong type, or the body is not
  UTF-8.
- `Blob` is the only `Value` that maps to a JSON array; JSON booleans and
  objects are rejected because SQL has no such value.
- `Value`, `Column` and `Table` no longer carry `Serialize` / `Deserialize`
  derives. Use the functions above (or `pager::save` / `pager::load`) to
  write or read the snapshot format.

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
- A `-wal` file with content returns `Err(SqlError::Unsupported)`.
- A leftover SQLKit `-journal` is rolled back before reading; a foreign journal returns `Err(SqlError::Unsupported)`.
- Valid magic with the `TSQL01` marker loads the legacy snapshot (migrated on the first write).
- Valid magic without the marker loads the foreign B-Tree file with rowids, schema texts, and plain index definitions; the next commit rewrites it in the native layout.

## Remaining Limits

- Written files always use page size `4096` and UTF-8; foreign page sizes and UTF-16 encodings stay readable but are normalized on the next commit.
- `WITHOUT ROWID` tables are rejected on read and never written.
- Exotic index definitions the parser rejects (partial, expression) stay in memory only and are omitted from the file; table rows stay complete.
- `ALTER TABLE` on a foreign table regenerates its stored `CREATE TABLE` text, normalizing exotic constraints; data is preserved.
- Rebuilds compact and preserve file size through the freelist trunk; freed pages are not yet reused as data pages (no growth cost, always valid).
- Files larger than 256 MiB are refused on read as a denial-of-service guard.

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
