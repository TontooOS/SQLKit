//! SQLKit performance harness (no third-party dependencies, only std).
//!
//! Run with: `cargo run --example perf` (use `--release` for fast numbers).
//!
//! Measures:
//!   (a) bulk insert of 10_000 rows via a reused prepared statement
//!   (b) full-table `query_map` scan over 10k rows
//!   (c) filtered SELECT with ORDER BY + LIMIT over 10k rows
//!   (d) file save + reload roundtrip with 10k rows
//!
//! Time budgets (generous, must also hold in debug builds on modest hardware;
//! debug is roughly 5-10x slower than release, hence the headroom):
//!   (a) bulk insert 10k rows ............ < 5_000 ms
//!   (b) full-table scan ................. < 5_000 ms
//!   (c) filtered ORDER BY + LIMIT ....... < 5_000 ms
//!   (d) file save + reload roundtrip .... < 15_000 ms
//! The harness asserts these budgets so regressions fail loudly.

use sqlkit::{params, Connection};
use std::time::Instant;

const ROWS: i64 = 10_000;

// Budgets in milliseconds (see module docs for rationale).
const BUDGET_INSERT_MS: u128 = 5_000;
const BUDGET_SCAN_MS: u128 = 5_000;
const BUDGET_FILTER_MS: u128 = 5_000;
const BUDGET_FILE_MS: u128 = 15_000;

fn setup_10k(conn: &Connection) {
    conn.execute_batch("CREATE TABLE bench (id INTEGER PRIMARY KEY, v INTEGER, name TEXT)")
        .unwrap();
    let mut stmt = conn
        .prepare("INSERT INTO bench (id, v, name) VALUES (?1, ?2, ?3)")
        .unwrap();
    for i in 0..ROWS {
        stmt.execute(params![i, i % 1000, format!("name-{i}")]).unwrap();
    }
}

fn bench_insert() -> u128 {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE bench (id INTEGER PRIMARY KEY, v INTEGER, name TEXT)")
        .unwrap();
    let mut stmt = conn
        .prepare("INSERT INTO bench (id, v, name) VALUES (?1, ?2, ?3)")
        .unwrap();
    let start = Instant::now();
    for i in 0..ROWS {
        stmt.execute(params![i, i % 1000, format!("name-{i}")]).unwrap();
    }
    let ms = start.elapsed().as_millis();
    let per_sec = ROWS as f64 / start.elapsed().as_secs_f64();
    println!("(a) bulk insert {ROWS} rows: {ms} ms ({per_sec:.0} rows/sec)");
    assert!(
        ms < BUDGET_INSERT_MS,
        "bulk insert budget exceeded: {ms} ms >= {BUDGET_INSERT_MS} ms"
    );
    ms
}

fn bench_scan() -> u128 {
    let conn = Connection::open_in_memory().unwrap();
    setup_10k(&conn);
    let mut stmt = conn.prepare("SELECT id, v FROM bench").unwrap();
    let start = Instant::now();
    let mut count = 0i64;
    let mut sum = 0i64;
    for row in stmt.query_map(params![], |row| row.get::<usize, i64>(1)).unwrap() {
        sum += row.unwrap();
        count += 1;
    }
    let ms = start.elapsed().as_millis();
    let per_sec = count as f64 / start.elapsed().as_secs_f64();
    println!("(b) full-table query_map scan: {ms} ms ({per_sec:.0} rows/sec, rows={count}, sum={sum})");
    assert_eq!(count, ROWS);
    assert!(
        ms < BUDGET_SCAN_MS,
        "scan budget exceeded: {ms} ms >= {BUDGET_SCAN_MS} ms"
    );
    ms
}

fn bench_filter_order_limit() -> u128 {
    let conn = Connection::open_in_memory().unwrap();
    setup_10k(&conn);
    let mut stmt = conn
        .prepare("SELECT v FROM bench WHERE v >= ?1 ORDER BY v DESC LIMIT 100")
        .unwrap();
    let start = Instant::now();
    let rows: Vec<i64> = stmt
        .query_map(params![500i64], |row| row.get(0))
        .unwrap()
        .collect::<sqlkit::Result<Vec<_>>>()
        .unwrap();
    let ms = start.elapsed().as_millis();
    let per_sec = ROWS as f64 / start.elapsed().as_secs_f64();
    println!(
        "(c) filtered ORDER BY + LIMIT: {ms} ms ({per_sec:.0} rows/sec scanned, returned={})",
        rows.len()
    );
    assert_eq!(rows.len(), 100);
    assert!(
        ms < BUDGET_FILTER_MS,
        "filter budget exceeded: {ms} ms >= {BUDGET_FILTER_MS} ms"
    );
    ms
}

fn bench_file_roundtrip() -> u128 {
    let path = std::env::temp_dir().join("sqlkit-perf-bench.sqlite");
    let _ = std::fs::remove_file(&path);
    let start = Instant::now();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE bench (id INTEGER PRIMARY KEY, v INTEGER, name TEXT)")
            .unwrap();
        // Bulk load inside a transaction so the file snapshot is written once
        // at commit instead of once per row.
        let tx = conn.transaction().unwrap();
        for i in 0..ROWS {
            tx.execute(
                "INSERT INTO bench (id, v, name) VALUES (?1, ?2, ?3)",
                params![i, i % 1000, format!("name-{i}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        conn.close().unwrap();
    }
    let save_ms = start.elapsed().as_millis();
    let conn = Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM bench", params![], |row| row.get(0))
        .unwrap();
    let total_ms = start.elapsed().as_millis();
    let per_sec = ROWS as f64 / start.elapsed().as_secs_f64();
    println!(
        "(d) file save + reload roundtrip: {total_ms} ms total (save {save_ms} ms, {per_sec:.0} rows/sec, rows={count})"
    );
    assert_eq!(count, ROWS);
    let _ = std::fs::remove_file(&path);
    assert!(
        total_ms < BUDGET_FILE_MS,
        "file roundtrip budget exceeded: {total_ms} ms >= {BUDGET_FILE_MS} ms"
    );
    total_ms
}

fn main() {
    println!("SQLKit perf harness: {ROWS} rows per benchmark");
    bench_insert();
    bench_scan();
    bench_filter_order_limit();
    bench_file_roundtrip();
    println!("All perf budgets met.");
}
