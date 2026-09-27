# Performance

Benchmark method, time budgets, and the hotspots fixed to keep bulk workloads fast without changing query semantics or the streaming guarantee.

## Benchmark Method

### `examples/perf.rs`

```rust
cargo run --example perf
```

Plain Rust harness with no third-party dependencies (`std::time::Instant` only). It measures four workloads over 10_000 rows each and prints timings plus throughput (rows/sec) to stdout:

| Workload | What it runs |
|---|---|
| `(a)` bulk insert | 10_000 single-row `INSERT` calls through one reused prepared statement (in-memory) |
| `(b)` full-table scan | `SELECT id, v FROM bench` drained through `query_map` with an index-based getter |
| `(c)` filtered ORDER BY + LIMIT | `SELECT v FROM bench WHERE v >= ?1 ORDER BY v DESC LIMIT 100` |
| `(d)` file roundtrip | 10_000 inserts inside one `transaction()`, `close()`, then `open()` + `SELECT COUNT(*)` |

- Returns `Err` behaviour: the harness panics (non-zero exit) when any budget in `Budgets` is exceeded, so regressions fail loudly in CI-style runs.
- File paths use `std::env::temp_dir()`; no `tempfile` needed outside `cargo test`.

## Budgets

Generous limits that must also hold in debug builds on modest hardware (debug is roughly 5-10x slower than release, hence the headroom):

| Workload | Budget (debug) | Current debug | Current release |
|---|---|---|---|
| `(a)` bulk insert 10k rows | `< 5_000 ms` | `37 ms` | `3 ms` |
| `(b)` full-table scan | `< 5_000 ms` | `4 ms` | `0 ms` |
| `(c)` filtered ORDER BY + LIMIT | `< 5_000 ms` | `1 ms` | `0 ms` |
| `(d)` file save + reload | `< 15_000 ms` | `108 ms` | `16 ms` |

Machine: 12th Gen Intel i5-12600K, 16 threads, 23 GB RAM, WSL Arch Linux, rustc 1.97.0. Throughput after the fix (debug): 264_166 rows/sec inserts, 2_062_691 rows/sec scans, 92_480 rows/sec file roundtrip.

Before the fix (same machine, debug): `(a)` 625 ms, `(b)` 6 ms, `(c)` 2 ms, `(d)` 80_227 ms (over the budget by 5x).

## Hotspots Fixed

### `Inner::pk_cache`

```rust
pub pk_cache: HashMap<String, HashMap<PkKey, usize>>
```

In-memory primary-key lookup cache (table key to key value to row position). The old `INSERT` path scanned `tbl.rows` linearly per row (`O(n)` per insert, `O(n^2)` per bulk load: 625 ms for 10k rows). Uniqueness checks are now `O(1)` (37 ms).

- Never serialized: the file format is unchanged.
- Built lazily from current rows on first primary-key insert per table.
- Invalidated (`remove` / `clear`) on `UPDATE`, `DELETE`, and transaction rollback.

### `Connection::persist_if_needed`

```rust
pub(crate) fn persist_if_needed(&self) -> Result<()>
```

Two fixes for the 80-second file roundtrip:

- Borrows `inner.tables` in place instead of cloning the whole database first.
- Returns early while `in_transaction` is set, so a 10_000-row bulk load inside `transaction()` writes exactly one snapshot at commit instead of one per row. The file always equals the last committed state.

`Transaction::commit` persists once after `commit_inner`; `Statement::execute` now persists file-backed writes exactly like `Connection::execute` (previously prepared writes skipped the snapshot entirely). `Begin` and `Rollback` no longer trigger snapshots: `Begin` only snapshots memory and `Rollback` restores memory to the already-persisted state.

### `ScanPlan`

```rust
struct ScanPlan { table_key: String, ncols: usize, out_columns: Vec<String>, proj: Option<Vec<usize>>, filter: Option<CompiledWhere> }
```

Compiled once per scan (lazily on first `next()` for `MappedRows`, eagerly for `query` / `collect_rows`):

- Table key lowercased once instead of per row.
- Column names resolved to positions once via `column_index_map`; bound parameters resolved once via `WhereClause::compile`. Per-row evaluation (`CompiledWhere::matches_row`) is infallible pure indexing with zero string comparisons and zero parameter lookups.
- Projection indices resolved once; the per-row `Vec<String>` schema clone is gone.

### `eval_like_borrowed`

```rust
pub(crate) fn eval_like_borrowed(actual: &Value, pattern: &Value) -> bool
```

`LIKE` evaluation on borrowed text (`Cow<str>`): `Text` and UTF-8 `Blob` values borrow in place instead of cloning a `String` per row. `eval_like` delegates to it with identical matching rules.

## Streaming Guarantee

`MappedRows` still never materializes the full result set:

- Without `ORDER BY` exactly one `Row` is materialized at a time; the borrow is dropped before the mapping closure runs, so the closure may write through the connection.
- With `ORDER BY` only the filtered rows are buffered for sorting; the table itself is never copied unfiltered.
- `SELECT COUNT(*)` still yields exactly one integer row.

Prefer index-based getters (`row.get(0)`) over name-based getters in hot loops: name lookup scans the column list per call by design.

## Usage / Example

```rust
cargo run --example perf
```

```text
SQLKit perf harness: 10000 rows per benchmark
(a) bulk insert 10000 rows: 37 ms (264166 rows/sec)
(b) full-table query_map scan: 4 ms (2062691 rows/sec, rows=10000, sum=4995000)
(c) filtered ORDER BY + LIMIT: 1 ms (5605208 rows/sec scanned, returned=100)
(d) file save + reload roundtrip: 108 ms total (save 98 ms, 92480 rows/sec, rows=10000)
All perf budgets met.
```

```rust
use sqlkit::{params, Connection};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE bench (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
let tx = conn.transaction().unwrap();
for i in 0..10_000i64 {
    tx.execute("INSERT INTO bench (id, v) VALUES (?1, ?2)", params![i, i]).unwrap();
}
tx.commit().unwrap();
```

## Cross References

- [Connection.md](Connection.md) – transactions and persistence behavior
- [Statement.md](Statement.md) – streaming `MappedRows` execution order
- [Pager.md](Pager.md) – atomic snapshot format
