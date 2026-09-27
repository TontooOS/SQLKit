//! Native B-Tree write path (milestone 2 interop).
//!
//! SQLKit writes byte-level real SQLite files: rusqlite reads back every
//! value, `PRAGMA integrity_check` passes, and the reverse direction (write
//! with rusqlite, modify with SQLKit, re-read with rusqlite) round-trips as
//! well. Journal recovery, WAL honesty, snapshot migration and freelist
//! reuse are covered below. The milestone 1 foreign-read tests stay green.

use rusqlite::types::Value as RValue;
use sqlkit::{Connection, Value};

const USERS: i64 = 300;
const ORDERS: i64 = 200;

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
        Some((0..(1 + i as usize % 17)).map(|k| ((i * 7 + k as i64) % 251) as u8).collect())
    };
    let nick = if i % 5 == 0 { "anon".to_string() } else { format!("nick{i}") };
    (id, name, score, note, avatar, nick)
}

fn expected_order(i: i64) -> (i64, i64, f64, Vec<u8>) {
    let payload =
        (0..(i as usize % 19)).map(|k| ((i * 13 + k as i64) % 251) as u8).collect();
    (1000 + i, (i % 300) + 1, i as f64 * 1.25 - 50.0, payload)
}

fn write_500_rows(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, note TEXT, avatar BLOB, nick TEXT DEFAULT 'anon'); \
         CREATE TABLE orders (oid INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount REAL NOT NULL, payload BLOB); \
         CREATE INDEX idx_orders_user ON orders (user_id);",
    )
    .unwrap();
    for i in 0..USERS {
        let user = expected_user(i);
        conn.execute(
            "INSERT INTO users (id, name, score, note, avatar, nick) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            sqlkit::params![user.0, user.1, user.2, user.3, user.4, user.5],
        )
        .unwrap();
    }
    for i in 0..ORDERS {
        let order = expected_order(i);
        conn.execute(
            "INSERT INTO orders (oid, user_id, amount, payload) VALUES (?1, ?2, ?3, ?4)",
            sqlkit::params![order.0, order.1, order.2, order.3],
        )
        .unwrap();
    }
}

fn approx_eq(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "float mismatch: {a} != {b}");
}

fn to_sqlkit(value: RValue) -> Value {
    match value {
        RValue::Null => Value::Null,
        RValue::Integer(v) => Value::Integer(v),
        RValue::Real(v) => Value::Real(v),
        RValue::Text(s) => Value::Text(s),
        RValue::Blob(b) => Value::Blob(b),
    }
}

fn rusqlite_row(db: &rusqlite::Connection, sql: &str) -> Vec<Value> {
    let mut stmt = db.prepare(sql).unwrap();
    let values: Vec<RValue> = stmt
        .query_row([], |row| {
            let count = row.as_ref().column_count();
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                out.push(row.get::<_, RValue>(i)?);
            }
            Ok(out)
        })
        .unwrap();
    values.into_iter().map(to_sqlkit).collect()
}

fn integrity_ok(path: &std::path::Path) {
    let db = rusqlite::Connection::open(path).unwrap();
    let check: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
    assert_eq!(check, "ok", "integrity_check failed for {}", path.display());
}

fn header_field(raw: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([raw[offset], raw[offset + 1], raw[offset + 2], raw[offset + 3]])
}

#[test]
fn native_roundtrip_500_rows_read_by_rusqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        write_500_rows(&conn);
        conn.close().unwrap();
    }
    // Real SQLite file, not a snapshot: magic plus no marker.
    let raw = std::fs::read(&path).unwrap();
    assert!(raw.starts_with(b"SQLite format 3\0"));
    assert!(!raw[100..].starts_with(b"TSQL01"));
    assert_eq!(u16::from_be_bytes([raw[16], raw[17]]), 4096);
    integrity_ok(&path);

    let db = rusqlite::Connection::open(&path).unwrap();
    let users: i64 = db.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0)).unwrap();
    let orders: i64 =
        db.query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0)).unwrap();
    assert_eq!((users, orders), (USERS, ORDERS));

    // Value-by-value equality over all 500 rows, both tables.
    for i in 0..USERS {
        let want = expected_user(i);
        let row = rusqlite_row(
            &db,
            &format!(
                "SELECT id, name, score, note, avatar, nick FROM users WHERE id = {}",
                want.0
            ),
        );
        assert_eq!(row[0], Value::Integer(want.0));
        assert_eq!(row[1], Value::Text(want.1));
        approx_eq(row[2].as_f64().unwrap(), want.2);
        assert_eq!(row[3], want.3.map(Value::Text).unwrap_or(Value::Null));
        assert_eq!(row[4], want.4.map(Value::Blob).unwrap_or(Value::Null));
        assert_eq!(row[5], Value::Text(want.5));
    }
    for i in 0..ORDERS {
        let want = expected_order(i);
        let row = rusqlite_row(
            &db,
            &format!("SELECT oid, user_id, amount, payload FROM orders WHERE oid = {}", want.0),
        );
        assert_eq!(row[0], Value::Integer(want.0));
        assert_eq!(row[1], Value::Integer(want.1));
        approx_eq(row[2].as_f64().unwrap(), want.2);
        let payload = match &row[3] {
            Value::Blob(b) => b.clone(),
            Value::Null if want.3.is_empty() => vec![],
            other => panic!("orders row {i}: bad payload {other:?}"),
        };
        assert_eq!(payload, want.3, "orders row {i}: payload");
    }

    // The file-side index answers queries (proves the index B-Tree is real).
    let via_index: i64 = db
        .query_row("SELECT COUNT(*) FROM orders WHERE user_id = 7", [], |row| row.get(0))
        .unwrap();
    assert_eq!(via_index, 1);

    // SQLKit reads its own file back identically.
    let conn = Connection::open(&path).unwrap();
    let back: String = conn
        .query_row("SELECT name FROM users WHERE id = ?1", sqlkit::params![42i64], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(back, expected_user(41).1);
    let big: String = conn
        .query_row("SELECT note FROM users WHERE id = ?1", sqlkit::params![300i64], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(big.len(), 100_000);
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM users", (), |row| row.get(0)).unwrap();
    assert_eq!(count, USERS);
}

#[test]
fn reverse_rusqlite_write_sqlkit_modify() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reverse.sqlite");
    // Real SQLite writes first, with scattered explicit rowids (gaps, large
    // varints) to pressure page splits and key ordering.
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, title TEXT NOT NULL, body TEXT, data BLOB); \
             CREATE TABLE tags (tid INTEGER PRIMARY KEY, doc_id INTEGER NOT NULL, label TEXT); \
             CREATE INDEX idx_tags_doc ON tags (doc_id);",
        )
        .unwrap();
        for i in 0..400i64 {
            let id = 3 * i + (i % 7) + if i % 50 == 0 { 1_000_000_000_000 } else { 0 };
            db.execute(
                "INSERT INTO docs (id, title, body, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    id,
                    format!("title {i}"),
                    if i % 4 == 0 { Option::<String>::None } else { Some(format!("body-{i}")) },
                    if i % 3 == 0 {
                        Option::<Vec<u8>>::None
                    } else {
                        Some(vec![i as u8; 1 + (i as usize % 25)])
                    },
                ],
            )
            .unwrap();
        }
        for i in 0..120i64 {
            db.execute(
                "INSERT INTO tags (tid, doc_id, label) VALUES (?1, ?2, ?3)",
                rusqlite::params![5000 + 2 * i, 3 * (i % 400), format!("tag-{i}")],
            )
            .unwrap();
        }
    }
    // SQLKit modifies: inserts (explicit + auto rowids), updates that grow
    // payloads into overflow, updates of the rowid alias itself, deletes.
    {
        let conn = Connection::open(&path).unwrap();
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM docs", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 400);
        conn.execute(
            "INSERT INTO docs (id, title, body) VALUES (?1, ?2, ?3)",
            sqlkit::params![9_999_999i64, "added", "new row"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO docs (title, body) VALUES (?1, ?2)",
            sqlkit::params!["auto row", Option::<String>::None],
        )
        .unwrap();
        let grown = conn
            .execute(
                "UPDATE docs SET body = ?1 WHERE title = ?2",
                sqlkit::params!["Y".repeat(20_000), "title 10"],
            )
            .unwrap();
        assert_eq!(grown, 1);
        // Change a rowid alias value: the B-Tree key must follow.
        let moved = conn
            .execute("UPDATE docs SET id = ?1 WHERE title = ?2", sqlkit::params![777i64, "title 11"])
            .unwrap();
        assert_eq!(moved, 1);
        let deleted = conn.execute("DELETE FROM docs WHERE title LIKE 'title 2%'", ()).unwrap();
        assert!(deleted >= 100, "expected bulk delete, got {deleted}");
        let removed_tags =
            conn.execute("DELETE FROM tags WHERE tid >= ?1", sqlkit::params![5100i64]).unwrap();
        assert!(removed_tags > 0);
        conn.execute(
            "INSERT INTO tags (tid, doc_id, label) VALUES (?1, ?2, ?3)",
            sqlkit::params![1i64, 777i64, "moved-tag"],
        )
        .unwrap();
        conn.close().unwrap();
    }
    integrity_ok(&path);
    // Re-read with real SQLite and verify the new state value by value.
    let db = rusqlite::Connection::open(&path).unwrap();
    let grown: String = db
        .query_row("SELECT body FROM docs WHERE title = 'title 10'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(grown.len(), 20_000);
    let moved: String =
        db.query_row("SELECT title FROM docs WHERE id = 777", [], |row| row.get(0)).unwrap();
    assert_eq!(moved, "title 11");
    let gone: i64 = db
        .query_row("SELECT COUNT(*) FROM docs WHERE title LIKE 'title 2%'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(gone, 0);
    let added: String =
        db.query_row("SELECT title FROM docs WHERE id = 9999999", [], |row| row.get(0)).unwrap();
    assert_eq!(added, "added");
    let auto: i64 = db
        .query_row("SELECT COUNT(*) FROM docs WHERE title = 'auto row'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto, 1);
    let tag: String =
        db.query_row("SELECT label FROM tags WHERE doc_id = 777", [], |row| row.get(0)).unwrap();
    assert_eq!(tag, "moved-tag");
    // SQLKit agrees with real SQLite after the round trip.
    let conn = Connection::open(&path).unwrap();
    let ours: i64 =
        conn.query_row("SELECT COUNT(*) FROM docs", (), |row| row.get(0)).unwrap();
    let theirs: i64 = db.query_row("SELECT COUNT(*) FROM docs", [], |row| row.get(0)).unwrap();
    assert_eq!(ours, theirs);
}

#[test]
fn journal_recovery_after_simulated_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", sqlkit::params![1i64, "one"])
            .unwrap();
        conn.close().unwrap();
    }
    // Halt the next write after the journal landed: temp image never
    // replaces the database, the journal stays behind.
    sqlkit::btree_write::set_crash_point_for(&path, Some("after_journal"));
    {
        let conn = Connection::open(&path).unwrap();
        let err = conn
            .execute("INSERT INTO t (id, v) VALUES (?1, ?2)", sqlkit::params![2i64, "two"])
            .expect_err("simulated crash must fail the write");
        assert!(format!("{err}").contains("simulated crash"));
    }
    let journal = {
        let mut name = path.as_os_str().to_owned();
        name.push("-journal");
        std::path::PathBuf::from(name)
    };
    assert!(journal.exists(), "journal must survive the simulated crash");
    // The database still holds the pre-crash state (real SQLite agrees).
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(n, 1);
    }
    // Reopening recovers the journal and keeps working.
    {
        let conn = Connection::open(&path).unwrap();
        assert!(!journal.exists(), "recovery must delete the journal");
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 1);
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", sqlkit::params![2i64, "two"])
            .unwrap();
        conn.close().unwrap();
    }
    assert!(!journal.exists());
    integrity_ok(&path);
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
    assert_eq!(n, 2);
}

#[test]
fn wal_file_refuses_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        conn.close().unwrap();
    }
    let wal = {
        let mut name = path.as_os_str().to_owned();
        name.push("-wal");
        std::path::PathBuf::from(name)
    };
    let shm = {
        let mut name = path.as_os_str().to_owned();
        name.push("-shm");
        std::path::PathBuf::from(name)
    };
    // A real uncheckpointed WAL: while the writer is alive the frames stay
    // live and SQLKit must refuse instead of reading stale rows.
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE w (id INTEGER PRIMARY KEY); INSERT INTO w (id) VALUES (1);",
        )
        .unwrap();
        assert!(wal.exists() && std::fs::metadata(&wal).unwrap().len() > 0);
        match Connection::open(&path) {
            Err(sqlkit::SqlError::Unsupported(_)) => {}
            Err(other) => panic!("expected Unsupported for live WAL, got {other}"),
            Ok(_) => panic!("live WAL must refuse the open"),
        }
    }
    // A stale WAL without a live writer refuses the same way.
    if !wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0 {
        std::fs::write(&wal, [vec![0x37u8, 0x7F, 0x06, 0x82], vec![0u8; 64]].concat()).unwrap();
    }
    let err = match Connection::open(&path) {
        Err(e) => e,
        Ok(_) => panic!("stale WAL must refuse the open"),
    };
    assert!(
        matches!(err, sqlkit::SqlError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
    std::fs::remove_file(&wal).unwrap();
    let _ = std::fs::remove_file(&shm);
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn snapshot_migrates_on_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    // Build a legacy snapshot file directly (header + TSQL01 + JSON).
    {
        let mut tables: std::collections::HashMap<String, sqlkit::Table> =
            std::collections::HashMap::new();
        tables.insert(
            "t".to_owned(),
            sqlkit::Table {
                name: "t".to_owned(),
                columns: vec![
                    sqlkit::Column {
                        name: "id".to_owned(),
                        coltype: "INTEGER".to_owned(),
                        primary_key: true,
                        not_null: false,
                        default: None,
                    },
                    sqlkit::Column {
                        name: "v".to_owned(),
                        coltype: "TEXT".to_owned(),
                        primary_key: false,
                        not_null: false,
                        default: None,
                    },
                ],
                rows: vec![vec![Value::Integer(1), Value::Text("old".to_owned())]],
            },
        );
        sqlkit::pager::save(&path, &tables).unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert!(raw[100..].starts_with(b"TSQL01"), "fixture must be a legacy snapshot");
    // Legacy read support: rows are visible before any write.
    {
        let conn = Connection::open(&path).unwrap();
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id = ?1", sqlkit::params![1i64], |row| row.get(0))
            .unwrap();
        assert_eq!(v, "old");
        // First write migrates to the native layout in the same directory.
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", sqlkit::params![2i64, "new"])
            .unwrap();
        conn.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert!(raw.starts_with(b"SQLite format 3\0"));
    assert!(!raw[100..].starts_with(b"TSQL01"), "snapshot must be gone after migration");
    assert!(!path.with_extension("sqlkit-tmp").exists(), "no temp file left behind");
    integrity_ok(&path);
    let db = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
    assert_eq!(n, 2);
    let v: String =
        db.query_row("SELECT v FROM t WHERE id = 2", [], |row| row.get(0)).unwrap();
    assert_eq!(v, "new");
}

#[test]
fn delete_keeps_size_via_freelist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("free.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)").unwrap();
        for i in 0..300i64 {
            conn.execute(
                "INSERT INTO t (id, body) VALUES (?1, ?2)",
                sqlkit::params![i, format!("row-{i}-{}", "p".repeat(200))],
            )
            .unwrap();
        }
        conn.close().unwrap();
    }
    let pages_before = std::fs::metadata(&path).unwrap().len() / 4096;
    assert!(pages_before > 3, "fixture needs several pages, got {pages_before}");
    {
        let conn = Connection::open(&path).unwrap();
        let removed = conn.execute("DELETE FROM t WHERE id >= ?1", sqlkit::params![10i64]).unwrap();
        assert_eq!(removed, 290);
        conn.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    let pages_after = raw.len() as u64 / 4096;
    assert_eq!(pages_after, pages_before, "shrinking must reuse the freelist, not truncate");
    // Header freelist trunk is linked: first trunk != 0 and total > 0.
    assert_ne!(header_field(&raw, 32), 0, "freelist trunk must be set");
    assert!(header_field(&raw, 36) > 0, "freelist must hold the freed pages");
    integrity_ok(&path);
    let db = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
    assert_eq!(n, 10);
}

#[test]
fn deep_tree_with_root_split_stays_valid() {
    // Wide rows force single-cell leaves, so the leaf level outgrows one
    // interior page: the writer must split interior pages and promote a new
    // root. One transaction keeps this to a single image rebuild.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE wide (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
        let tx = conn.transaction().unwrap();
        for i in 0..1_500i64 {
            tx.execute(
                "INSERT INTO wide (id, v) VALUES (?1, ?2)",
                sqlkit::params![i, format!("v{i:05}-{}", "z".repeat(2000))],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        conn.close().unwrap();
    }
    let pages = std::fs::metadata(&path).unwrap().len() / 4096;
    assert!(pages > 500, "fixture needs a multi-level tree, got {pages} pages");
    integrity_ok(&path);
    let db = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = db.query_row("SELECT COUNT(*) FROM wide", [], |row| row.get(0)).unwrap();
    assert_eq!(n, 1_500);
    // Point lookups descend every level of the new tree.
    for id in [0i64, 1, 749, 1498, 1499] {
        let v: String =
            db.query_row("SELECT v FROM wide WHERE id = ?1", rusqlite::params![id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(v.starts_with(&format!("v{id:05}-")), "row {id}");
        assert_eq!(v.len(), 7 + 2000);
    }
    let conn = Connection::open(&path).unwrap();
    let back: String = conn
        .query_row("SELECT v FROM wide WHERE id = ?1", sqlkit::params![0i64], |row| row.get(0))
        .unwrap();
    assert_eq!(back.len(), 7 + 2000);
}

#[test]
fn schema_cookie_bumps_on_schema_change_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cookie.sqlite");
    let cookie = || header_field(&std::fs::read(&path).unwrap(), 40);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE a (id INTEGER PRIMARY KEY)").unwrap();
        let first = cookie();
        conn.execute("INSERT INTO a (id) VALUES (?1)", sqlkit::params![1i64]).unwrap();
        assert_eq!(cookie(), first, "data writes must not bump the cookie");
        conn.execute_batch("CREATE TABLE b (id INTEGER PRIMARY KEY)").unwrap();
        assert!(cookie() > first, "CREATE TABLE must bump the cookie");
        conn.execute_batch("CREATE INDEX idx_a ON a (id)").unwrap();
        let after_index = cookie();
        conn.execute("UPDATE a SET id = ?1 WHERE id = ?2", sqlkit::params![2i64, 1i64])
            .unwrap();
        assert_eq!(cookie(), after_index, "data writes must not bump the cookie");
        conn.close().unwrap();
    }
    integrity_ok(&path);
}
