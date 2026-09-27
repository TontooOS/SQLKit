//! Foreign SQLite read path (B-Tree pager milestone 1).
//!
//! Builds a REAL SQLite file with CPython `sqlite3` (500 rows across two
//! tables, plus an index, an `AUTOINCREMENT` sequence table, and one
//! overflow-sized `TEXT`), then asserts SQLKit opens it and returns
//! identical rows value-by-value, including `JOIN` and `GROUP BY` over the
//! foreign file. Writes keep using the snapshot format and are out of scope.

use sqlkit::{Connection, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const USERS: i64 = 300;
const ORDERS: i64 = 200;

const BUILD_SCRIPT: &str = r#"
import sqlite3
import sys

db = sqlite3.connect(sys.argv[1])
c = db.cursor()
c.execute("CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, score REAL, note TEXT, avatar BLOB, nick TEXT DEFAULT 'anon')")
c.execute("CREATE TABLE orders (oid INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount REAL NOT NULL, payload BLOB)")
c.execute("CREATE INDEX idx_orders_user ON orders(user_id)")
for i in range(300):
    name = "user-%04d-\u00e4\u00f6\u00fc" % i
    score = i * 0.5 - 10.0
    if i % 3 == 0:
        note = None
    elif i == 299:
        note = "X" * 100000
    else:
        note = "note %d with 'quotes' and check \u2713 %d" % (i, i)
    avatar = None if i % 2 == 1 else bytes(((i * 7 + k) % 251 for k in range(1 + i % 17)))
    if i % 5 == 0:
        c.execute("INSERT INTO users (name, score, note, avatar) VALUES (?,?,?,?)", (name, score, note, avatar))
    else:
        c.execute("INSERT INTO users (name, score, note, avatar, nick) VALUES (?,?,?,?,?)", (name, score, note, avatar, "nick%d" % i))
for i in range(200):
    c.execute(
        "INSERT INTO orders (oid, user_id, amount, payload) VALUES (?,?,?,?)",
        (1000 + i, (i % 300) + 1, i * 1.25 - 50.0, bytes(((i * 13 + k) % 251 for k in range(i % 19)))),
    )
db.commit()
db.close()
"#;

/// Build the foreign fixture with CPython `sqlite3`; returns the file path.
fn build_fixture(dir: &tempfile::TempDir) -> PathBuf {
    let script = dir.path().join("build_foreign.py");
    let db = dir.path().join("foreign.sqlite");
    std::fs::write(&script, BUILD_SCRIPT).unwrap();
    let out = Command::new("python3")
        .arg(&script)
        .arg(&db)
        .output()
        .expect("python3 must be available to build the foreign fixture");
    assert!(
        out.status.success(),
        "fixture build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists(), "python3 did not create the fixture file");
    db
}

fn expected_user(i: i64) -> (i64, String, f64, Option<String>, Option<Vec<u8>>, String) {
    let id = i + 1;
    let name = format!("user-{i:04}-\u{e4}\u{f6}\u{fc}");
    let score = i as f64 * 0.5 - 10.0;
    let note = if i % 3 == 0 {
        None
    } else if i == 299 {
        Some("X".repeat(100_000))
    } else {
        Some(format!("note {i} with 'quotes' and check \u{2713} {i}"))
    };
    let avatar = if i % 2 == 1 {
        None
    } else {
        Some(
            (0..(1 + i as usize % 17))
                .map(|k| ((i * 7 + k as i64) % 251) as u8)
                .collect(),
        )
    };
    let nick = if i % 5 == 0 { "anon".to_string() } else { format!("nick{i}") };
    (id, name, score, note, avatar, nick)
}

fn expected_order(i: i64) -> (i64, i64, f64, Vec<u8>) {
    let payload = (0..(i as usize % 19))
        .map(|k| ((i * 13 + k as i64) % 251) as u8)
        .collect();
    (1000 + i, (i % 300) + 1, i as f64 * 1.25 - 50.0, payload)
}

fn approx_eq(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "float mismatch: {a} != {b}");
}

fn open_foreign(path: &Path) -> Connection {
    // Sanity: the fixture is a real foreign file, not a SQLKit snapshot.
    let raw = std::fs::read(path).unwrap();
    assert!(raw.starts_with(b"SQLite format 3\0"));
    assert!(!raw[100..].starts_with(b"TSQL01"));
    Connection::open(path).unwrap()
}

#[test]
fn foreign_file_reads_500_rows_value_by_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = build_fixture(&dir);

    let (tables, stats) = sqlkit::btree::load_foreign_with_stats(&db).unwrap();
    assert_eq!(stats.tables, 2);
    assert_eq!(stats.rows, 500);
    assert_eq!(tables.len(), 2);

    let conn = open_foreign(&db);

    let users: Vec<(i64, String, f64, Option<String>, Option<Vec<u8>>, String)> = conn
        .prepare("SELECT id, name, score, note, avatar, nick FROM users ORDER BY id ASC")
        .unwrap()
        .query_map(sqlkit::params![], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .unwrap()
        .collect::<sqlkit::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(users.len(), USERS as usize);
    for (i, got) in users.iter().enumerate() {
        let want = expected_user(i as i64);
        assert_eq!((got.0, &got.1), (want.0, &want.1), "users row {i}: id/name");
        approx_eq(got.2, want.2);
        assert_eq!((&got.3, &got.4, &got.5), (&want.3, &want.4, &want.5), "users row {i}");
    }

    let orders: Vec<(i64, i64, f64, Vec<u8>)> = conn
        .prepare("SELECT oid, user_id, amount, payload FROM orders ORDER BY oid ASC")
        .unwrap()
        .query_map(sqlkit::params![], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .unwrap()
        .collect::<sqlkit::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(orders.len(), ORDERS as usize);
    for (i, got) in orders.iter().enumerate() {
        let want = expected_order(i as i64);
        assert_eq!((got.0, got.1), (want.0, want.1), "orders row {i}");
        approx_eq(got.2, want.2);
        assert_eq!(got.3, want.3, "orders row {i}: payload");
    }

    let users_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", (), |row| row.get(0))
        .unwrap();
    let orders_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM orders", (), |row| row.get(0))
        .unwrap();
    assert_eq!((users_count, orders_count), (USERS, ORDERS));

    // `sqlite_sequence` (from AUTOINCREMENT) and the index carry no user
    // rows: the internal table stays invisible to the engine.
    assert!(conn.query_row("SELECT COUNT(*) FROM sqlite_sequence", (), |_| Ok(())).is_err());
}

#[test]
fn foreign_file_join_group_by_and_rowid_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = build_fixture(&dir);
    let conn = open_foreign(&db);

    // Every one of the first 200 users owns exactly one order.
    let joined: Vec<(String, i64, f64)> = conn
        .prepare(
            "SELECT users.name, COUNT(orders.oid), SUM(orders.amount) FROM users \
             INNER JOIN orders ON users.id = orders.user_id \
             GROUP BY users.name ORDER BY users.name ASC",
        )
        .unwrap()
        .query_map(sqlkit::params![], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, f64>(2)?))
        })
        .unwrap()
        .collect::<sqlkit::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(joined.len(), ORDERS as usize);
    for (i, (name, count, sum)) in joined.iter().enumerate() {
        let want_user = expected_user(i as i64);
        let want_order = expected_order(i as i64);
        assert_eq!(name, &want_user.1);
        assert_eq!(*count, 1);
        approx_eq(*sum, want_order.2);
    }

    let grouped: Vec<(i64, f64)> = conn
        .prepare(
            "SELECT COUNT(*), SUM(orders.amount) FROM orders \
             GROUP BY orders.user_id HAVING COUNT(*) >= 1",
        )
        .unwrap()
        .query_map(sqlkit::params![], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })
        .unwrap()
        .collect::<sqlkit::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(grouped.len(), ORDERS as usize);
    let total: f64 = grouped.iter().map(|(_, sum)| sum).sum();
    let want_total: f64 = (0..ORDERS).map(|i| expected_order(i).2).sum();
    approx_eq(total, want_total);

    // Interior-page descent to a single rowid, plus a missing-rowid miss.
    let row = sqlkit::btree::find_rowid(&db, "users", 42).unwrap().expect("rowid 42 exists");
    assert_eq!(row[0], Value::Integer(42));
    assert_eq!(row[1], Value::Text(expected_user(41).1));
    assert!(sqlkit::btree::find_rowid(&db, "users", 1_000_000).unwrap().is_none());
    assert!(sqlkit::btree::find_rowid(&db, "no_such_table", 1).is_err());
}
