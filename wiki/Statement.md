# Statement

Prepared statement with ruslite-style execution and a streaming row iterator that never materializes the full result set.

## Execution

### `execute`

```rust
pub fn execute(&mut self, params: impl IntoParams) -> Result<usize>
```

Runs a write statement with bound parameters and returns rows changed.

- Returns `Err(SqlError::ExecuteReturnedResults)` when the statement is a `SELECT`.
- Returns `Err(SqlError::InvalidParameterCount)` on missing bindings.

```rust
let mut stmt = conn.prepare("UPDATE t SET v = ?1 WHERE id = ?2").unwrap();
stmt.execute(params![2i32, "a"]).unwrap();
```

### `query_row`

```rust
pub fn query_row<T, P, F>(&mut self, params: P, f: F) -> Result<T>
```

Maps the first result row with `f`. Returns `Err(SqlError::QueryReturnedNoRows)` when the result is empty.

```rust
let name: String = stmt.query_row(params!["a"], |row| row.get(0)).unwrap();
```

### `query_map`

```rust
pub fn query_map<T, P, F>(&mut self, params: P, f: F) -> Result<MappedRows<'_, F>>
```

Returns a lazy `Iterator<Item = Result<T>>` over mapped rows. Exactly one row is materialized at a time.

Execution order is filter, then sort, then `OFFSET` / `LIMIT`:

- Without `ORDER BY` the scan stays fully lazy: the filter evaluates per row, `OFFSET` skips matching rows, and the stream ends after `LIMIT` rows.
- With `ORDER BY` only the filtered rows are buffered for sorting; the table itself is never copied unfiltered.
- `SELECT COUNT(*)` yields exactly one integer row (single `COUNT(*)` column); `ORDER BY`, `LIMIT`, and `OFFSET` are ignored for counts.

```rust
let names: Vec<String> = stmt
    .query_map(params![0i32], |row| row.get(0))
    .unwrap()
    .collect::<Result<Vec<_>>>()
    .unwrap();
```

```rust
let mut stmt = conn.prepare("SELECT name FROM t ORDER BY age DESC LIMIT 5 OFFSET 10").unwrap();
let page: Vec<String> = stmt
    .query_map(params![], |row| row.get(0))
    .unwrap()
    .collect::<Result<Vec<_>>>()
    .unwrap();
```

### `query`

```rust
pub fn query(&mut self, params: impl IntoParams) -> Result<Vec<Row>>
```

Collects all rows. Prefer `query_map` for large result sets.

### `column_count`

```rust
pub fn column_count(&self) -> usize
```

Returns the projected column count for `SELECT` lists, or `0` for `SELECT *` until the pager follow-up resolves star widths at prepare time.

## Row Access

### `Row::get`

```rust
pub fn get<I, T>(&self, idx: I) -> Result<T>
```

Reads a column by `0`-based index (`usize`) or by name (`&str`, `String`) into any `T: FromValue`.

- Returns `Err(SqlError::InvalidColumnIndex)` on out-of-range positions.
- Returns `Err(SqlError::InvalidColumnName)` on unknown names.
- Returns `Err(SqlError::FromSqlConversionFailure)` on type mismatch.

## Usage / Example

```rust
use sqlkit::{params, Connection};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").unwrap();
conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params!["a", 1i32]).unwrap();
let mut stmt = conn.prepare("SELECT v FROM t WHERE id = ?1").unwrap();
let v: i32 = stmt.query_row(params!["a"], |row| row.get(0)).unwrap();
```

## Cross References

- [Connection.md](Connection.md) – preparing statements
- [Value.md](Value.md) – value conversions and parameters
- [Parser.md](Parser.md) – supported SELECT filters
