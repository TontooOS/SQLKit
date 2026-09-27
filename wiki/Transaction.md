# Transaction

RAII transaction handle with commit and rollback.

## Lifecycle

```rust
pub fn transaction(&self) -> Result<Transaction<'_>>
```

Snapshots all tables on begin. Nested transactions return `Err(SqlError::Transaction)`.

### `commit`

```rust
pub fn commit(mut self) -> Result<()>
```

Discards the snapshot and keeps all writes. Consumes the handle.

### `rollback`

```rust
pub fn rollback(mut self) -> Result<()>
```

Restores the pre-transaction tables and discards writes. Consumes the handle.

Dropping a `Transaction` without `commit` or `rollback` rolls back automatically. The `Drop` implementation never panics; errors during implicit rollback are ignored.

## Transaction Queries

| Method | Description |
|---|---|
| `execute` | Write inside the transaction |
| `execute_batch` | Batch inside the transaction |
| `prepare` | Prepare a statement on the parent connection |
| `query_row` | Read inside the transaction |

## Usage / Example

```rust
use sqlkit::Connection;

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").unwrap();
let tx = conn.transaction().unwrap();
tx.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", ("a", 1i32)).unwrap();
tx.commit().unwrap();
```

## Cross References

- [Connection.md](Connection.md) – starting transactions
- [Pager.md](Pager.md) – persistence after commit
