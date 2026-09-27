# Tontoo SQLKit – Wiki

Dependency-light SQLite-compatible engine for TontooOS with a ruslite-compatible API, streaming iterators, and atomic file snapshots.

- Repository: https://github.com/TontooOS/SQLKit
- License: TCL v26.1
- Version: 26.1.0

## Feature Index

| Feature | File | Description |
|---|---|---|
| Main index | [MAIN.md](MAIN.md) | This page |
| Rules | [RULE.md](RULE.md) | Wiki design system |
| Connection | [Connection.md](Connection.md) | Open, execute, execute_batch, prepare, transactions |
| Statement | [Statement.md](Statement.md) | Prepared statements, Row, streaming MappedRows |
| Value | [Value.md](Value.md) | Value, ToSql, FromValue, IntoParams, params macro |
| Parser | [Parser.md](Parser.md) | Supported SQL subset and grammar limits |
| Pager | [Pager.md](Pager.md) | File format, atomic snapshots, foreign-file policy |
| Transaction | [Transaction.md](Transaction.md) | RAII transactions with commit and rollback |
| FFI | [FFI.md](FFI.md) | C header and memory rules |
| Performance | [Performance.md](Performance.md) | Benchmark method, budgets, and hotspot fixes |

## Quick Start

```rust
use sqlkit::{params, Connection};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT)").unwrap();
conn.execute(
    "INSERT INTO notes (id, title) VALUES (?1, ?2)",
    params!["a", "Hello"],
)
.unwrap();
let title: String = conn
    .query_row("SELECT title FROM notes WHERE id = ?1", params!["a"], |row| row.get(0))
    .unwrap();
assert_eq!(title, "Hello");
```

```c
SQLKitConnection *db = sqlkit_open_memory();
sqlkit_exec_batch(db, "CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT)");
sqlkit_close(db);
```

See [Connection.md](Connection.md) for details.

## Scope and Roadmap

Basis engine implements `CREATE TABLE`, `CREATE INDEX`, `INSERT` with
`OR REPLACE` and `OR IGNORE`, `SELECT` with `WHERE` filters, `JOIN`,
sub-selects, aggregates, `GROUP BY` / `HAVING`, `DISTINCT`, `UNION`,
`ORDER BY`, `LIMIT` / `OFFSET`, `UPDATE`, `DELETE`, `PRAGMA`, and
transactions. Foreign SQLite B-Tree files are readable (milestone 1 read
path: header, table pages, records, overflow chains, schema discovery);
writes always persist the snapshot format, and native B-Tree writes are
milestone 2 work. Triggers and views are follow-up items. See
[Pager.md](Pager.md) and [Parser.md](Parser.md) for the exact limits.

## Changelog

- 2026-09-27: Performance pass 2: per-connection parsed-statement cache, shared column storage, scratch row reuse, position-based ORDER BY window projection, COUNT(*) fast path, borrow-based prepared writes; SQLKit now beats rusqlite 0.32 in 4 of 5 identical-workload areas (file open stays an honest pager loss); 7 new regression tests; documented in Performance.md.
- 2026-09-27: B-Tree pager milestone 1: `src/btree.rs` reads foreign SQLite files (header, table pages, records, overflow chains, schema discovery, 256 MiB guard); `Connection::open` loads them, writes keep the snapshot format; covered by `tests/foreign_read.rs` (500 rows, JOIN, GROUP BY).
- 2026-09-27: Performance pass: added `examples/perf.rs` harness with budgets, PK lookup cache, deferred transaction snapshots, compiled scan plans, borrow-based LIKE; documented in Performance.md.
- 2026-09-27: Initial wiki created with all 7 feature pages.
