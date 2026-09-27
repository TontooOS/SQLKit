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
| `(a)` bulk insert 10k rows | `< 5_000 ms` | `25 ms` | `3 ms` |
| `(b)` full-table scan | `< 5_000 ms` | `3 ms` | `0 ms` |
| `(c)` filtered ORDER BY + LIMIT | `< 5_000 ms` | `0 ms` | `0 ms` |
| `(d)` file save + reload | `< 15_000 ms` | `69 ms` | `7 ms` |

Machine: WSL Arch Linux, rustc 1.97.0. Throughput after the second pass (debug):
387_588 rows/sec inserts, 2_546_440 rows/sec scans, 142_907 rows/sec file roundtrip.

Before the fix (same machine, debug): `(a)` 625 ms, `(b)` 6 ms, `(c)` 2 ms, `(d)` 80_227 ms (over the budget by 5x).

## SQLKit vs rusqlite

Identical workloads (`N = 100_000`, release mode) run against SQLKit and rusqlite 0.32
(bundled SQLite) measure where the engine stands. In-memory for A-C, file-backed with one
transaction for D:

| Area | rusqlite (before) | sqlkit (before) | rusqlite (after) | sqlkit (after) |
|---|---|---|---|---|
| `A` bulk insert 100k (1 tx) | `103.75 ms` | `131.25 ms` | `113.29 ms` | `37.96 ms` |
| `B` full scan 100k | `8.76 ms` | `15.78 ms` | `8.75 ms` | `7.38 ms` |
| `C` filter + ORDER BY + LIMIT 100 | `7.02 ms` | `9.29 ms` | `12.85 ms` | `2.19 ms` |
| `D1` file open 100k | `0.04 ms` | `7.97 ms` | `0.04 ms` | `10.60 ms` |
| `D2` COUNT(*) 100k | `0.39 ms` | `7.76 ms` | `0.37 ms` | `0.02 ms` |

SQLKit now wins 4 of 5 areas (A, B, C, D2). `D1` stays an honest loss by design: rusqlite
memory-maps the file with a lazy pager while SQLKit parses the full snapshot on open, so an
open can never beat a pager without skipping work; the file format and the full parse are
unchanged.

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

### `Connection::stmt_cache`

```rust
stmt_cache: RefCell<HashMap<String, Stmt>>
```

Parsed-statement cache keyed by the exact SQL text, held in its own `RefCell` next to
`inner`. Parsing depends only on the SQL string, never on schema or data state, so entries
never go stale. `Connection::execute` runs directly from the cache borrow (no parse, no
statement clone) because `run_stmt` borrows the disjoint `inner` cell; `prepare` clones the
cached statement once. Bounded at 256 entries (cleared when full, correctness unaffected).
Bulk inserts through one reused SQL string went from 131 ms to 38 ms per 100k rows.

### `SharedColumns`

```rust
pub(crate) type SharedColumns = Rc<Vec<String>>
```

`Row.columns` and `ScanPlan.out_columns` share one column-name allocation per query by
pointer instead of cloning the name strings per row. Removes one `Vec` plus one `String`
clone per column per row from full-table scans (100k rows by 3 columns: 300k `String`
clones gone). `Row::column_names` still returns `&[String]`; the public API is unchanged.

### `MappedRows::scratch`

The lazy single-table path refills one reused `Row` buffer (`project_planned_into`) instead
of allocating a new `Row` per iteration. Safe because the mapping closure receives the row by
reference and returns an owned value before the next refill, and the table borrow still ends
before the closure runs, so the closure may write through the connection.

### Position-based `ORDER BY` window

```rust
fn sort_row_positions(table_rows: &[Vec<Value>], table_cols: &[String], positions: &mut [usize], order_by: &[OrderBy]) -> Result<()>
```

`ORDER BY` paths (`collect_rows`, the single-table fast path, `MappedRows::init_sorted`) sort
matching row positions with the same comparator and the same stable sort as before, then trim
to the `OFFSET` / `LIMIT` window and project only the surviving rows. Filtered
`ORDER BY v DESC LIMIT 100` over 100k rows went from 9.3 ms to 2.2 ms because 50k full rows
(including the untouched text column) are no longer cloned for sorting.

### `COUNT(*)` fast path

`SELECT COUNT(*)` without `GROUP BY` / `HAVING` / `DISTINCT` (guaranteed by the parser) now
routes through the compiled `collect_rows` counter instead of cloning every row twice through
the general aggregate path. Counts matching rows with zero row clones: 7.8 ms down to
0.02 ms per 100k rows. Queries with sub-select filters keep the general path.

### Borrowed prepared writes

`Statement::execute` runs `run_stmt` against the stored statement by reference instead of
cloning it per call; the clone showed up in bulk-write profiles and was never needed.

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
(a) bulk insert 10000 rows: 3 ms (2721298 rows/sec)
(b) full-table query_map scan: 0 ms (20859060 rows/sec, rows=10000, sum=4995000)
(c) filtered ORDER BY + LIMIT: 0 ms (36688227 rows/sec scanned, returned=100)
(d) file save + reload roundtrip: 7 ms total (save 5 ms, 1380652 rows/sec, rows=10000)
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
