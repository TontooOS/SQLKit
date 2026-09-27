# Connection

Database handle with a ruslite-compatible subset. Owns the table store and persists file-backed databases atomically.

## Constructors

### `Connection::open`

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Self>
```

Opens or creates a file-backed database. Creates parent directories as needed. Validates the SQLite magic on existing files and loads the SQLKit snapshot when present.

- Returns `Err(SqlError::Unsupported)` when the file is a foreign SQLite B-Tree file without a SQLKit snapshot.
- Returns `Err(SqlError::NotSqliteFile)` when the magic is missing.
- Returns `Err(SqlError::Io)` on filesystem failures.

```rust
let conn = Connection::open("/tmp/app.sqlite").unwrap();
```

### `Connection::open_in_memory`

```rust
pub fn open_in_memory() -> Result<Self>
```

Opens a transient database with no file persistence. Never returns an I/O error in practice.

```rust
let conn = Connection::open_in_memory().unwrap();
```

## Execution

### `execute`

```rust
pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize>
```

Runs exactly one write statement and returns rows changed. Accepts `params![...]`, `Vec<Value>`, tuples, `[Value; N]`, and `()`.

- Returns `Err(SqlError::MultipleStatement)` on batches.
- Returns `Err(SqlError::ExecuteReturnedResults)` on `SELECT`.
- Returns `Err(SqlError::InvalidParameterCount)` on unbound placeholders.

```rust
let changed = conn.execute("UPDATE t SET v = ?1 WHERE id = ?2", params![1i32, "a"]).unwrap();
```

### `execute_batch`

```rust
pub fn execute_batch(&self, sql: &str) -> Result<()>
```

Runs one or more statements without parameters. Accepts `PRAGMA` and transaction control words. Persists once after the whole batch.

```rust
conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
```

### `prepare`

```rust
pub fn prepare(&self, sql: &str) -> Result<Statement<'_>>
```

Prepares one statement for repeated execution. See [Statement.md](Statement.md).

### `query_row`

```rust
pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
```

Prepares, runs a `SELECT`, and maps the first row. Returns `Err(SqlError::QueryReturnedNoRows)` when empty.

## Transactions and Maintenance

### `transaction`

```rust
pub fn transaction(&self) -> Result<Transaction<'_>>
```

Begins a transaction. See [Transaction.md](Transaction.md).

### `changes`

```rust
pub fn changes(&self) -> usize
```

Returns rows changed by the last write on this connection.

### `checkpoint`

```rust
pub fn checkpoint(&self) -> Result<()>
```

Records a WAL checkpoint marker and persists. Basis behavior is a validated no-op.

### `close`

```rust
pub fn close(self) -> Result<()>
```

Flushes the snapshot to disk and releases the handle.

## Usage / Example

```rust
use sqlkit::{params, Connection};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").unwrap();
conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params!["a", 1i32]).unwrap();
let tx = conn.transaction().unwrap();
tx.execute("UPDATE t SET v = ?1 WHERE id = ?2", params![2i32, "a"]).unwrap();
tx.commit().unwrap();
```

## Cross References

- [Statement.md](Statement.md) – prepared statements and row mapping
- [Transaction.md](Transaction.md) – commit and rollback behavior
- [Pager.md](Pager.md) – file persistence and foreign-file policy
