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

| Area | rusqlite | sqlkit | Result |
|---|---|---|---|
| `A` bulk insert 100k (1 tx) | `120.05 ms` | `56.77 ms` | sqlkit ~2.1x faster |
| `B` full scan 100k | `9.98 ms` | `7.48 ms` | sqlkit ~1.3x faster |
| `C` filter + ORDER BY + LIMIT 100 | `8.41 ms` | `2.73 ms` | sqlkit ~3.1x faster |
| `D1` file open 100k | `0.04 ms` | `0.03 ms` | sqlkit faster (lazy open) |
| `D2` COUNT(*) 100k | `0.38 ms` | `0.32 ms` | sqlkit ~1.2x faster |

SQLKit now wins all 5 areas. History: after pass 2 it won 4 of 5 (`D1`
stayed an honest loss: full snapshot parse on open vs the lazy pager).
Pass 3 closed the gap with lazy open (below) and the unmaterialized cell
count (below); the `A` regression of the native-write milestone
(O(rowids) scan per insert) was fixed with the cached maximum (below).

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

### Lazy open

```rust
pub(crate) fn ensure_loaded(&self) -> Result<()>
```

File-backed connections start unloaded: `open` only runs the WAL check,
journal recovery, and a 110-byte header/marker peek (`pager::peek_kind`),
so opening is O(1) regardless of file size. The full load happens on the
first data access (`execute`, `execute_batch`, `prepare` stays lazy and
loads in the `Statement` methods, `transaction` loads before snapshotting
so rollback restores real data). Corrupt content therefore surfaces on
first use, not on open. `close` on an untouched connection rewrites
nothing (see the dirty flag below).

### `Inner::rowid_max`

```rust
pub rowid_max: HashMap<String, i64>
```

Cached per-table rowid maximum. The native-write milestone regressed bulk
inserts to O(rows) per row by scanning the rowid vector for `max + 1`
(3.4 s per 100k rows); the cache restores O(1) allocation (57 ms).
Maintained on insert (explicit values raise it), recomputed from surviving
ids on `UPDATE` / `DELETE`, seeded on load and `CREATE TABLE`, saved and
restored by transaction backup/rollback. Saturation at `i64::MAX` falls
back to the smallest free positive rowid (`smallest_free_rowid`,
`SQLITE_FULL` code 13 when truly exhausted).

### Dirty flag

```rust
pub dirty: bool
```

`run_stmt` marks durable state changed for every write statement;
`persist_if_needed` skips the file rewrite when nothing changed. A freshly
opened connection that is only read or closed never touches the file.

### Unmaterialized `COUNT(*)`

`Connection::fast_count_star` answers plain `COUNT(*)` over an unloaded
real B-Tree file with `btree::count_table_rows`: a cell-pointer walk that
sums leaf cell counts without decoding any record (allocation-free apart
from one bitset and the image buffer, which is pre-sized from the file
metadata). Unknown tables error without loading the file; snapshot files
and filtered queries take the normal path.

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
