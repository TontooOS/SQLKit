//! Lazy open, unmaterialized `COUNT(*)`, rowid-max tracking and the dirty
//! flag: fresh-connection semantics without eager loading.

use sqlkit::{params, Connection};

fn seed(path: &std::path::Path, rows: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
        .unwrap();
    let tx = conn.transaction().unwrap();
    for i in 1..=rows {
        tx.execute(
            "INSERT INTO t (id, v) VALUES (?1, ?2)",
            params![i, i * 10],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn open_then_close_does_not_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clean.sqlite");
    seed(&path, 10);
    let before = std::fs::read(&path).unwrap();
    // Open (lazy: no data load) and close without touching data.
    Connection::open(&path).unwrap().close().unwrap();
    let after = std::fs::read(&path).unwrap();
    assert_eq!(before, after, "read-only open+close rewrote the file");
}

#[test]
fn corruption_surfaces_on_first_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bait.sqlite");
    seed(&path, 5);
    // Corrupt the payload behind a valid header.
    let mut bytes = std::fs::read(&path).unwrap();
    for b in bytes.iter_mut().skip(500) {
        *b = 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();
    // Lazy open still succeeds; the error surfaces on first data access.
    let conn = Connection::open(&path).unwrap();
    assert!(conn.query_row("SELECT COUNT(*) FROM t", params![], |row| row.get::<_, i64>(0)).is_err());
}

#[test]
fn count_star_over_unloaded_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("count.sqlite");
    seed(&path, 250);
    let conn = Connection::open(&path).unwrap();
    // Answered by the B-Tree cell walk, then normal queries still load.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", params![], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 250);
    let v: i64 = conn
        .query_row("SELECT v FROM t WHERE id = ?1", params![7i64], |row| row.get(0))
        .unwrap();
    assert_eq!(v, 70);
}

#[test]
fn count_star_unknown_table_errors_without_loading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.sqlite");
    seed(&path, 5);
    let conn = Connection::open(&path).unwrap();
    let err = conn
        .query_row("SELECT COUNT(*) FROM nope", params![], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_err();
    assert!(format!("{err}").contains("no such table"));
}

#[test]
fn rowid_max_tracks_delete_and_reuses() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    for i in 1..=5 {
        conn.execute("INSERT INTO t (id, v) VALUES (?1, 'x')", params![i as i64]).unwrap();
    }
    conn.execute("DELETE FROM t WHERE id = ?1", params![5i64]).unwrap();
    // SQLite reuse semantics: next id is max(remaining) + 1 = 5.
    conn.execute("INSERT INTO t (v) VALUES ('y')", params![]).unwrap();
    let id: i64 = conn
        .query_row("SELECT id FROM t WHERE v = 'y'", params![], |row| row.get(0))
        .unwrap();
    assert_eq!(id, 5);
}

#[test]
fn rowid_max_survives_rollback() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    conn.execute("INSERT INTO t (id, v) VALUES (10, 'a')", params![]).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO t (id, v) VALUES (11, 'b')", params![]).unwrap();
    tx.rollback().unwrap();
    // Auto id continues after the rolled-back value: max(10) + 1 = 11.
    conn.execute("INSERT INTO t (v) VALUES ('c')", params![]).unwrap();
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM t ORDER BY id").unwrap();
        stmt.query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(ids, vec![10, 11]);
}
