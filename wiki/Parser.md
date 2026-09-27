# Parser

Minimal SQL parser for the basis engine and its exact limits.

## Supported Statements

| Statement      | Example                                                  | Notes                                                                                       |
|----------------|----------------------------------------------------------|-----------------------------------------------------------------------------------------------|
| `PRAGMA`       | `PRAGMA journal_mode=WAL`                                | Stored in memory; `wal_checkpoint(TRUNCATE)` persists                                        |
| `CREATE TABLE` | `CREATE TABLE t (id TEXT PRIMARY KEY)`                   | `IF NOT EXISTS`, `PRIMARY KEY`, `NOT NULL`, `DEFAULT lit`                                    |
| `CREATE INDEX` | `CREATE INDEX i ON t (col)`                              | `IF NOT EXISTS`; metadata only, no optimizer use yet                                         |
| `INSERT`       | `INSERT OR REPLACE INTO t (a) VALUES (?1)`               | Multi-row `VALUES` lists; `OR REPLACE`, `OR IGNORE`                                          |
| `SELECT`       | `SELECT a FROM t WHERE b = ?1 ORDER BY a DESC LIMIT 5`   | `*`, column list, aggregates, `DISTINCT`, `JOIN`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT` / `OFFSET`, `UNION` |
| `UNION`        | `SELECT a FROM t UNION ALL SELECT a FROM u`              | `UNION` (dedup) and `UNION ALL` over two `SELECT`s with matching widths                      |
| `ALTER TABLE`  | `ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'`            | `ADD COLUMN c TYPE [NOT NULL] [DEFAULT lit]`; existing rows get `NULL` or the default        |
| `UPDATE`       | `UPDATE t SET a = ?1 WHERE id = ?2`                      | Optional `WHERE`; returns rows changed                                                       |
| `DELETE`       | `DELETE FROM t WHERE id = ?1`                            | Optional `WHERE`; returns rows removed                                                       |
| `BEGIN`        | `BEGIN`                                                  | Only inside `execute_batch` or `Transaction`                                                 |
| `COMMIT`       | `COMMIT`                                                 | Commits the current transaction                                                              |
| `ROLLBACK`     | `ROLLBACK`                                               | Restores the pre-transaction snapshot                                                        |

Placeholders are `?`, `?N`, `:name`, and `@name`. Named parameters bind positionally in order of appearance, exactly like `?`. Literals are `NULL`, integers, floats, `'text'` with `''` escapes, `TRUE`, and `FALSE`.

## WHERE Filters

Each condition compares one column against an expression (literal or bound parameter):

| Form         | Example                        | Notes                                                                                       |
|--------------|--------------------------------|-----------------------------------------------------------------------------------------------|
| Comparison   | `age >= ?1`, `name <> 'x'`     | `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`; `NULL` on either side never matches                   |
| `LIKE`       | `name LIKE 'A%'`               | Case-sensitive; `%` spans any run, `_` matches one character; `NOT LIKE` negates             |
| `IN`         | `age IN (20, 25, ?1)`          | List must not be empty; `NOT IN` negates; `col IN (SELECT ...)` uses a sub-select             |
| Scalar equal | `age = (SELECT age FROM u)`    | Single-row sub-select expected; multi-row is an execution error; empty yields `NULL` (no match) |
| `IS NULL`    | `nick IS NULL`                 | True only for `NULL` values                                                                 |
| `IS NOT NULL`| `nick IS NOT NULL`             | True only for non-`NULL` values                                                             |

Conditions chain with `AND` and `OR`. `AND` binds tighter than `OR`, so `a = 1 OR b = 2 AND c = 3` parses as `a = 1 OR (b = 2 AND c = 3)`. Column names may be qualified as `table.col`; plain names match the first column with that name.

## JOIN

```rust
parse_one("SELECT a.x, b.y FROM a INNER JOIN b ON a.x = b.y").unwrap();
parse_one("SELECT a.x FROM a LEFT JOIN b ON a.x = b.y").unwrap();
```

- `FROM a [INNER | LEFT] JOIN b ON a.x = b.y` with chained joins allowed. Plain `JOIN` means `INNER`. `LEFT [OUTER] JOIN` is accepted.
- `ON` conditions are column equality only (`=`); other operators are a parse error.
- The select list may use qualified `table.col` names. `SELECT *` over a join exposes qualified `table.col` columns.
- `LEFT JOIN` pads missing right sides with `NULL`. Non-grouped joins stream one row at a time; `ORDER BY` over a join buffers only the filtered joined rows.

## Aggregates, GROUP BY, HAVING

```rust
parse_one("SELECT COUNT(*), SUM(pages) FROM books").unwrap();
parse_one("SELECT author, COUNT(*) FROM books GROUP BY author HAVING COUNT(*) > 5").unwrap();
```

- Aggregates without `GROUP BY`: `COUNT(*)`, `COUNT(col)`, `SUM(col)`, `AVG(col)`, `MIN(col)`, `MAX(col)`. Existing `COUNT(*)` behavior is preserved (one integer row, `ORDER BY` / `LIMIT` ignored; other pure aggregates behave the same).
- `COUNT(col)` counts non-`NULL` values; `SUM` / `AVG` / `MIN` / `MAX` skip `NULL`. Empty input yields `0` for `COUNT` and `NULL` otherwise. `SUM` returns `Integer` for all-integer input and `Real` otherwise; `AVG` always returns `Real`.
- `GROUP BY col [, ...]` (plain or qualified) with the aggregates above plus `HAVING` (aggregate or column predicate with `AND` / `OR`). Grouping buffers only the filtered rows of its query.
- Output column names are `COUNT(*)`, `SUM(col)`, and similar (qualified as `SUM(t.col)` when qualified).

## DISTINCT and UNION

```rust
parse_one("SELECT DISTINCT age FROM t").unwrap();
parse_one("SELECT age FROM t UNION ALL SELECT age FROM u").unwrap();
```

- `SELECT DISTINCT` (including `SELECT DISTINCT *`) deduplicates rows via a seen set while streaming.
- `UNION` (dedup) and `UNION ALL` over two `SELECT`s with matching widths. Width mismatch is an execution error (`SqliteFailure`). Each side keeps its own `WHERE` / `GROUP BY` / `ORDER BY` / `LIMIT`.

## ALTER TABLE

```rust
parse_one("ALTER TABLE t ADD COLUMN c TEXT NOT NULL DEFAULT 'x'").unwrap();
```

- `ALTER TABLE t ADD COLUMN c TYPE [NOT NULL] [DEFAULT lit]` where `lit` is `NULL`, an integer, float, text, `TRUE`, or `FALSE`.
- Existing rows get `NULL` or the default. New `INSERT`s without the column use the default when present, otherwise `NULL`.
- Duplicate column names and `PRIMARY KEY` in `ADD COLUMN` are errors.

## ORDER BY, LIMIT, COUNT

`SELECT` accepts an optional ordering and window after the filter:

```rust
parse_one("SELECT name FROM t WHERE age >= 20 ORDER BY age DESC, name ASC LIMIT 10 OFFSET 5").unwrap();
parse_one("SELECT COUNT(*) FROM t WHERE age >= 20").unwrap();
```

- `ORDER BY col [ASC | DESC] [, ...]` sorts by table columns (plain or `table.col`, default `ASC`). `ORDER BY` keys may reference columns outside the select list. Grouped queries sort by output columns.
- `LIMIT n` caps the rows returned; `OFFSET m` skips the first `m` rows. Both accept integer literals or bound parameters. A negative `LIMIT` means no upper bound; a negative `OFFSET` clamps to zero. `OFFSET` without `LIMIT` is a parse error.
- `SELECT COUNT(*) FROM t [WHERE ...]` returns exactly one integer row with a single `COUNT(*)` column. `ORDER BY`, `LIMIT`, and `OFFSET` are accepted but ignored for `COUNT(*)`.

## Limits

The parser rejects triggers and views with `Err(SqlError::Unsupported)`. `INSERT ... SELECT`, foreign-key enforcement, and the full B-Tree page layout remain roadmap items.

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
