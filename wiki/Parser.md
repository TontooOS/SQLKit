# Parser

Minimal SQL parser for the basis engine and its exact limits.

## Supported Statements

| Statement | Example | Notes |
|---|---|---|
| `PRAGMA` | `PRAGMA journal_mode=WAL` | Stored in memory; `wal_checkpoint(TRUNCATE)` persists |
| `CREATE TABLE` | `CREATE TABLE t (id TEXT PRIMARY KEY)` | `IF NOT EXISTS`, `PRIMARY KEY`, `NOT NULL`, `DEFAULT` |
| `CREATE INDEX` | `CREATE INDEX i ON t (col)` | `IF NOT EXISTS`; metadata only, no optimizer use yet |
| `INSERT` | `INSERT OR REPLACE INTO t (a) VALUES (?1)` | Multi-row `VALUES` lists; `OR REPLACE`, `OR IGNORE` |
| `SELECT` | `SELECT a FROM t WHERE b = ?1` | `*` or column list; `AND`-chained `col = ?` filters |
| `UPDATE` | `UPDATE t SET a = ?1 WHERE id = ?2` | Optional `WHERE`; returns rows changed |
| `DELETE` | `DELETE FROM t WHERE id = ?1` | Optional `WHERE`; returns rows removed |
| `BEGIN` | `BEGIN` | Only inside `execute_batch` or `Transaction` |
| `COMMIT` | `COMMIT` | Commits the current transaction |
| `ROLLBACK` | `ROLLBACK` | Restores the pre-transaction snapshot |

Placeholders are `?`, `?N`, and named positions are not yet supported. Literals are `NULL`, integers, floats, `'text'` with `''` escapes, `TRUE`, and `FALSE`.

## Limits

The parser rejects JOIN, sub-selects, `UNION`, aggregates, `GROUP BY`, `ORDER BY`, `LIMIT`, triggers, views, `ALTER TABLE`, and named parameters with `Err(SqlError::Unsupported)`. These are assigned to the query-language follow-up subagent.

### `parse_one`

```rust
pub fn parse_one(sql: &str) -> Result<Stmt>
```

Parses exactly one statement. Returns `Err(SqlError::MultipleStatement)` on batches and `Err(SqlError::Parse)` on syntax errors.

### `parse_batch`

```rust
pub fn parse_batch(sql: &str) -> Result<Vec<Stmt>>
```

Splits on `;` outside string literals and parses each part. Used by `execute_batch`.

## Usage / Example

```rust
use sqlkit::parser::{parse_batch, parse_one};

let stmt = parse_one("SELECT data FROM objects WHERE entity = ?1").unwrap();
let batch = parse_batch("PRAGMA foreign_keys=ON; CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
assert_eq!(batch.len(), 2);
```

## Cross References

- [Connection.md](Connection.md) – how parsed statements execute
- [Statement.md](Statement.md) – supported SELECT filters
