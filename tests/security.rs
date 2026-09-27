//! Security regression tests: malformed SQL, corrupted files, hostile
//! integers, FFI null handling, sidecar symlinks and file permissions.
//!
//! Every test asserts graceful `Err` / error codes — never a panic — on
//! adversarial input.

use sqlkit::parser::{MAX_BATCH_STATEMENTS, MAX_NESTING_DEPTH};
use sqlkit::{params, Connection, SqlError};

// ---------------------------------------------------------------------------
// Malformed SQL: parse errors, never panics.
// ---------------------------------------------------------------------------

#[test]
fn garbage_sql_is_rejected() {
    let cases = [
        "",
        ";",
        "   ;  ;",
        "SELECT",
        "SELECT * FROM",
        "SELECT 'abc",
        "SELECT \"abc",
        "INSERT INTO",
        "CREATE TABLE",
        "PRAGMA",
        "\u{0}SELECT 1",
        "\u{fffd}\u{fffd}",
        "SELECT 1; SELECT 2",
        "((((((((((SELECT 1))))))))))",
        "SELECT ?0 FROM t",
        "SELECT ?1000 FROM t",
    ];
    for sql in cases {
        assert!(sqlkit::parser::parse_one(sql).is_err(), "should reject: {sql:?}");
    }
}

#[test]
fn oversized_statement_is_rejected() {
    let big = format!("SELECT '{}'", "x".repeat(2_000_000));
    let err = sqlkit::parser::parse_one(&big).unwrap_err();
    assert!(matches!(err, SqlError::Parse(_)));
    // Engine entry points enforce the same bound.
    let conn = Connection::open_in_memory().unwrap();
    assert!(conn.execute(&big, params![]).is_err());
    assert!(conn.execute_batch(&big).is_err());
}

#[test]
fn oversized_batch_is_rejected() {
    let batch = "SELECT 1;".repeat(MAX_BATCH_STATEMENTS + 10);
    assert!(sqlkit::parser::parse_batch(&batch).is_err());
    let wide = " ".repeat(9_000_000);
    assert!(sqlkit::parser::parse_batch(&wide).is_err());
}

#[test]
fn deep_nesting_is_rejected() {
    // 200-deep sub-select nesting (limit is MAX_NESTING_DEPTH).
    let mut sql = String::from("SELECT 1");
    for _ in 0..MAX_NESTING_DEPTH + 70 {
        sql = format!("SELECT * FROM ({sql})");
    }
    assert!(sqlkit::parser::parse_one(&sql).is_err());
}

#[test]
fn long_chains_are_rejected() {
    let ands = format!("SELECT a FROM t WHERE {}", vec!["a = 1"; 300].join(" AND "));
    assert!(sqlkit::parser::parse_one(&ands).is_err());
    let ors = format!("SELECT a FROM t WHERE {}", vec!["a = 1"; 300].join(" OR "));
    assert!(sqlkit::parser::parse_one(&ors).is_err());
    let unions = format!("SELECT a FROM t {}", "UNION SELECT a FROM t ".repeat(300));
    assert!(sqlkit::parser::parse_one(&unions).is_err());
    let joins = format!("SELECT a FROM t {}", "JOIN u ON t.a = u.a ".repeat(300));
    assert!(sqlkit::parser::parse_one(&joins).is_err());
    let groups = format!(
        "SELECT a FROM t GROUP BY {}",
        (0..300).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", ")
    );
    assert!(sqlkit::parser::parse_one(&groups).is_err());
}

#[test]
fn too_many_placeholders_are_rejected() {
    let values = vec!["?"; 1500].join(", ");
    let sql = format!("INSERT INTO t VALUES ({values})");
    assert!(sqlkit::parser::parse_one(&sql).is_err());
}

#[test]
fn long_identifier_does_not_panic() {
    // A 1 MiB identifier stays under the statement cap: must parse-or-error,
    // never panic.
    let sql = format!("SELECT {} FROM t", "a".repeat(1_000_000));
    let _ = sqlkit::parser::parse_one(&sql);
}

// ---------------------------------------------------------------------------
// Corrupted files: corrupt errors, never panics.
// ---------------------------------------------------------------------------

fn header_template() -> [u8; 100] {
    let mut header = [0u8; 100];
    header[0..16].copy_from_slice(b"SQLite format 3\0");
    header[16..18].copy_from_slice(&4096u16.to_be_bytes());
    header[18] = 1;
    header[19] = 1;
    header[56..60].copy_from_slice(&1u32.to_be_bytes());
    header
}

#[test]
fn bad_headers_are_rejected() {
    // Wrong magic.
    let mut bad = header_template();
    bad[0] = b'X';
    assert!(sqlkit::btree::parse_file_header(&bad).is_err());
    // Truncated.
    assert!(sqlkit::btree::parse_file_header(&bad[..50]).is_err());
    // A large-but-legal reserved-bytes value parses (usable stays >= 64).
    let mut reserved = header_template();
    reserved[20] = 200;
    assert!(sqlkit::btree::parse_file_header(&reserved).is_ok());
    // Unknown text encoding.
    let mut enc = header_template();
    enc[56..60].copy_from_slice(&99u32.to_be_bytes());
    assert!(sqlkit::btree::parse_file_header(&enc).is_err());
}

#[test]
fn truncated_records_are_rejected() {
    assert!(sqlkit::btree::parse_record(&[], sqlkit::btree::TextEncoding::Utf8).is_err());
    assert!(sqlkit::btree::parse_record(&[0xFF], sqlkit::btree::TextEncoding::Utf8).is_err());
    // Reserved serial types 10/11.
    assert!(sqlkit::btree::parse_record(&[2, 10], sqlkit::btree::TextEncoding::Utf8).is_err());
    assert!(sqlkit::btree::parse_record(&[2, 11], sqlkit::btree::TextEncoding::Utf8).is_err());
    // Declared-giant TEXT with no body: errors fast without allocating.
    let serial_len = 1u64 << 40;
    let serial = 13 + 2 * serial_len;
    let mut payload = vec![0u8; 10];
    payload[0] = 10; // header length claims 10 bytes
    let varint = sqlkit::btree_write::encode_varint(serial);
    payload[1..1 + varint.len()].copy_from_slice(&varint);
    let before = std::time::Instant::now();
    assert!(sqlkit::btree::parse_record(&payload, sqlkit::btree::TextEncoding::Utf8).is_err());
    assert!(before.elapsed().as_secs() < 5);
}

/// Minimal two-page file whose table leaf cell carries rowid `u64::MAX`.
/// Must surface a range error, never wrap or panic.
fn max_rowid_file() -> Vec<u8> {
    fn varint(mut v: u64) -> Vec<u8> {
        let mut groups = vec![(v & 0x7F) as u8];
        v >>= 7;
        while v > 0 {
            groups.push((v & 0x7F) as u8);
            v >>= 7;
        }
        groups.reverse();
        // Continuation bit on every byte but the last (values here are all
        // small; large varints are hardcoded byte-wise below).
        for i in 0..groups.len().saturating_sub(1) {
            groups[i] |= 0x80;
        }
        groups
    }
    fn record(fields: &[Vec<u8>], serials: &[u64]) -> Vec<u8> {
        let mut header = Vec::new();
        for s in serials {
            header.extend(varint(*s));
        }
        let mut hlen = varint(header.len() as u64 + 1);
        let mut out = Vec::new();
        out.append(&mut hlen);
        out.extend(header);
        for f in fields {
            out.extend(f);
        }
        out
    }
    fn leaf_page(cells: &[Vec<u8>], base: usize) -> Vec<u8> {
        let mut page = vec![0u8; 4096];
        page[base] = 0x0D;
        page[base + 3..base + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
        let mut cursor = 4096usize;
        for (i, cell) in cells.iter().enumerate() {
            cursor -= cell.len();
            page[base + 8 + 2 * i..base + 8 + 2 * i + 2]
                .copy_from_slice(&(cursor as u16).to_be_bytes());
            page[cursor..cursor + cell.len()].copy_from_slice(cell);
        }
        page[base + 5..base + 7].copy_from_slice(&(cursor as u16).to_be_bytes());
        page
    }
    let sql = b"CREATE TABLE t(a TEXT)";
    let master = record(
        &[b"table".to_vec(), b"t".to_vec(), b"t".to_vec(), vec![2], sql.to_vec()],
        &[13 + 2 * 5, 13 + 2 * 1, 13 + 2 * 1, 1, 13 + 2 * sql.len() as u64],
    );
    let mut master_cell = varint(master.len() as u64);
    master_cell.extend(varint(1));
    master_cell.extend(master);
    // Table leaf cell: payload_len = 1, rowid = u64::MAX (9-byte varint),
    // one body byte. Must surface a range error, never wrap.
    let mut leaf_cell = varint(1);
    leaf_cell.extend(vec![0xFF; 8]);
    leaf_cell.push(0xFF);
    leaf_cell.push(0x00);
    let mut file = header_template().to_vec();
    file.extend(&leaf_page(&[master_cell], 100)[100..]);
    file.extend(&leaf_page(&[leaf_cell], 0)[0..]);
    file
}

#[test]
fn rowid_out_of_range_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maxrowid.sqlite");
    std::fs::write(&path, max_rowid_file()).unwrap();
    let err = match Connection::open(&path) {
        Ok(_) => panic!("crafted max-rowid file opened without error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("rowid") || msg.contains("range") || msg.contains("corrupt") || msg.contains("failure"),
        "unexpected error: {msg}"
    );
}

#[test]
fn truncated_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short.sqlite");
    std::fs::write(&path, b"SQLite format 3\0partial").unwrap();
    assert!(Connection::open(&path).is_err());
}

// ---------------------------------------------------------------------------
// Hostile integers: saturation, no wrap.
// ---------------------------------------------------------------------------

#[test]
fn max_rowid_inserts_do_not_wrap() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    conn.execute(
        "INSERT INTO t (id, v) VALUES (?1, ?2)",
        params![i64::MAX, "max"],
    )
    .unwrap();
    // Next auto rowid must not panic or wrap to negative.
    conn.execute("INSERT INTO t (v) VALUES (?1)", params!["next"]).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", params![], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[test]
fn error_messages_truncate_hostile_values() {
    let evil = "x".repeat(10_000);
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (txt TEXT)").unwrap();
    conn.execute("INSERT INTO t (txt) VALUES (?1)", params![evil]).unwrap();
    let err = conn
        .query_row("SELECT txt FROM t", params![], |row| row.get::<_, i64>(0))
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("[truncated]"), "long value not truncated: {msg:?}");
    assert!(msg.len() < 1000, "error leaked long value: {} chars", msg.len());
}

// ---------------------------------------------------------------------------
// Unicode schema through the lenient path (sized type forces fallback).
// ---------------------------------------------------------------------------

#[test]
fn unicode_schema_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unicode.sqlite");
    {
        let rc = rusqlite::Connection::open(&path).unwrap();
        rc.execute_batch(
            "CREATE TABLE \"tëst\" (\"nâme\" VARCHAR(10), \"🎃\" INTEGER DEFAULT 7); \
             INSERT INTO \"tëst\" (\"nâme\", \"🎃\") VALUES ('héllo', 42);",
        )
        .unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    let name: String = conn
        .query_row("SELECT \"nâme\" FROM \"tëst\"", params![], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "héllo");
}

// ---------------------------------------------------------------------------
// FFI: null handling, no crashes.
// ---------------------------------------------------------------------------

#[test]
fn ffi_null_arguments_are_safe() {
    use std::ffi::CString;
    unsafe {
        assert!(sqlkit::ffi::sqlkit_open(std::ptr::null()).is_null());
        assert!(sqlkit::ffi::sqlkit_exec_batch(std::ptr::null_mut(), std::ptr::null()) < 0);
        assert!(sqlkit::ffi::sqlkit_exec(std::ptr::null_mut(), std::ptr::null()) < 0);
        sqlkit::ffi::sqlkit_close(std::ptr::null_mut());
        sqlkit::ffi::sqlkit_free_string(std::ptr::null_mut());
        assert!(!sqlkit::ffi::sqlkit_version().is_null());

        let mem = sqlkit::ffi::sqlkit_open_memory();
        assert!(!mem.is_null());
        // Null SQL string.
        assert!(sqlkit::ffi::sqlkit_exec_batch(mem, std::ptr::null()) < 0);
        assert!(sqlkit::ffi::sqlkit_exec(mem, std::ptr::null()) < 0);
        // Valid roundtrip through C.
        let ddl = CString::new("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
        assert_eq!(sqlkit::ffi::sqlkit_exec_batch(mem, ddl.as_ptr()), 0);
        let bad = CString::new("BOGUS SQL !!!").unwrap();
        assert!(sqlkit::ffi::sqlkit_exec_batch(mem, bad.as_ptr()) < 0);
        let one = CString::new("INSERT INTO t (id) VALUES ('x')").unwrap();
        assert!(sqlkit::ffi::sqlkit_exec(mem, one.as_ptr()) >= 0);
        sqlkit::ffi::sqlkit_close(mem);
    }
}

// ---------------------------------------------------------------------------
// Filesystem: permissions and symlink refusal (unix only).
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn database_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("perms.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
    conn.execute("INSERT INTO t (id) VALUES ('secret')", params![]).unwrap();
    drop(conn);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "database file mode is {mode:o}, expected 600");
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} has mode {mode:o}", entry.path().display());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_journal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("victim.sqlite");
    let elsewhere = dir.path().join("elsewhere.bin");
    std::fs::write(&elsewhere, b"precious").unwrap();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
        conn.execute("INSERT INTO t (id) VALUES ('a')", params![]).unwrap();
    }
    // No journal present: open succeeds. The attacker plants a symlink where
    // the journal will go *after* the open (TOCTOU); the write must refuse
    // it instead of following the link.
    let conn = Connection::open(&path).unwrap();
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    let journal = std::path::PathBuf::from(journal);
    std::os::unix::fs::symlink(&elsewhere, &journal).unwrap();
    let err = conn.execute("INSERT INTO t (id) VALUES ('b')", params![]).unwrap_err();
    assert!(format!("{err}").contains("symlink"), "unexpected error: {err}");
    drop(conn);
    // Target untouched, database still consistent after removing the link.
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"precious");
    std::fs::remove_file(&journal).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute("INSERT INTO t (id) VALUES ('b')", params![]).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", params![], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 2);
}
