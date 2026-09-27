# Parser

Minimal SQL parser for the basis engine and its exact limits.

## Supported Statements

| Statement | Example | Notes |
|---|---|---|
| `PRAGMA` | `PRAGMA journal_mode=WAL` | Stored in memory; `wal_checkpoint(TRUNCATE)` persists |
| `CREATE TABLE` | `CREATE TABLE t (id TEXT PRIMARY KEY)` | `IF NOT EXISTS`, `PRIMARY KEY`, `NOT NULL`, `DEFAULT` |
| `CREATE INDEX` | `CREATE INDEX i ON t (col)` | `IF NOT EXISTS`; metadata only, no optimizer use yet |
| `INSERT` | `INSERT OR REPLACE INTO t (a) VALUES (?1)` | Multi-row `VALUES` lists; `OR REPLACE`, `OR IGNORE` |
| `SELECT` | `SELECT a FROM t WHERE b = ?1 ORDER BY a DESC LIMIT 5` | `*` or column list, or `COUNT(*)`; `WHERE` with comparison operators, `LIKE`, `IN`, `IS NULL`, `AND` / `OR`; optional `ORDER BY` and `LIMIT` / `OFFSET` |
| `UPDATE` | `UPDATE t SET a = ?1 WHERE id = ?2` | Optional `WHERE`; returns rows changed |
| `DELETE` | `DELETE FROM t WHERE id = ?1` | Optional `WHERE`; returns rows removed |
| `BEGIN` | `BEGIN` | Only inside `execute_batch` or `Transaction` |
| `COMMIT` | `COMMIT` | Commits the current transaction |
| `ROLLBACK` | `ROLLBACK` | Restores the pre-transaction snapshot |

Placeholders are `?`, `?N`, `:name`, and `@name`. Named parameters bind positionally in order of appearance, exactly like `?`. Literals are `NULL`, integers, floats, `'text'` with `''` escapes, `TRUE`, and `FALSE`.

## WHERE Filters

Each condition compares one column against an expression (literal or bound parameter):

| Form | Example | Notes |
|---|---|---|
| Comparison | `age >= ?1`, `name <> 'x'`, `id != ?1` | `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`; `NULL` on either side never matches |
| `LIKE` | `name LIKE 'A%'` | Case-sensitive; `%` spans any run, `_` matches one character; `NOT LIKE` negates |
| `IN` | `age IN (20, 25, ?1)` | List must not be empty; `NOT IN` negates |
| `IS NULL` | `nick IS NULL` | True only for `NULL` values |
| `IS NOT NULL` | `nick IS NOT NULL` | True only for non-`NULL` values |

Conditions chain with `AND` and `OR`. `AND` binds tighter than `OR`, so `a = 1 OR b = 2 AND c = 3` parses as `a = 1 OR (b = 2 AND c = 3)`.

## ORDER BY, LIMIT, COUNT

`SELECT` accepts an optional ordering and window after the filter:

```rust
parse_one("SELECT name FROM t WHERE age >= 20 ORDER BY age DESC, name ASC LIMIT 10 OFFSET 5").unwrap();
parse_one("SELECT COUNT(*) FROM t WHERE age >= 20").unwrap();
```

- `ORDER BY col [ASC | DESC] [, ...]` sorts by table columns (default `ASC`). `ORDER BY` keys may reference columns outside the select list.
- `LIMIT n` caps the rows returned; `OFFSET m` skips the first `m` rows. Both accept integer literals or bound parameters. A negative `LIMIT` means no upper bound; a negative `OFFSET` clamps to zero. `OFFSET` without `LIMIT` is a parse error.
- `SELECT COUNT(*) FROM t [WHERE ...]` returns exactly one integer row with a single `COUNT(*)` column. `ORDER BY`, `LIMIT`, and `OFFSET` are accepted but ignored for `COUNT(*)`.

## Limits

The parser rejects JOIN, sub-selects, `UNION`, aggregates other than `COUNT(*)`, `GROUP BY`, triggers, views, and `ALTER TABLE` with `Err(SqlError::Unsupported)`. These are assigned to the query-language follow-up subagent.

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
