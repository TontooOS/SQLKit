//! File pager: SQLite header handling, legacy snapshots, and open helpers.
//!
//! Milestone 2 scope: every database file SQLKit creates is a real native
//! SQLite database (100-byte header, `sqlite_master` on page 1, table and
//! index B-Trees; see [`crate::btree_write`]). Files with the legacy
//! `TSQL01` JSON snapshot marker are still readable exactly as before, but
//! the first write migrates them to the native layout through an atomic
//! temp-file rename in the same directory. Foreign SQLite B-Tree files are
//! readable through the milestone 1 read path (see [`crate::btree`]).
//!
//! Crash safety uses rollback-journal mode and WAL files with uncheckpointed
//! frames refuse the open; both live in [`crate::btree_write`]. I/O stays
//! streaming-oriented: the file is replaced atomically (temp file + fsync +
//! rename), so readers never see a half-written database.

use crate::connection::Table;
use crate::error::{Result, SqlError};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
pub const HEADER_LEN: usize = 100;
pub const DEFAULT_PAGE_SIZE: u16 = 4096;
pub const SQLKIT_MARKER: &[u8; 6] = b"TSQL01";
pub const SNAPSHOT_VERSION: u32 = 1;

fn build_header() -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..16].copy_from_slice(SQLITE_MAGIC);
    header[16..18].copy_from_slice(&DEFAULT_PAGE_SIZE.to_be_bytes());
    header[18] = 1;
    header[19] = 1;
    header[20] = 0;
    header[21] = 64;
    header[22] = 32;
    header[23] = 32;
    header
}

fn parse_header(header: &[u8]) -> Result<u32> {
    if header.len() < HEADER_LEN {
        return Err(SqlError::NotSqliteFile("file smaller than SQLite header".into()));
    }
    if &header[0..16] != SQLITE_MAGIC {
        return Err(SqlError::NotSqliteFile("missing SQLite magic".into()));
    }
    let raw = u16::from_be_bytes([header[16], header[17]]);
    Ok(if raw == 1 { 65536 } else { u32::from(raw.max(512)) })
}

pub fn file_exists(path: &Path) -> bool {
    path.exists()
}

/// Validate an existing file: it must carry the SQLite magic. Returns the
/// page size on success.
pub fn validate(path: &Path) -> Result<u32> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    parse_header(&header)
}

/// Cheap backing-layout probe for lazy open: validates the magic and peeks
/// at the snapshot marker (110 bytes, no full read). Snapshot files load
/// through the legacy path; everything else with SQLite magic goes through
/// the B-Tree reader (native and foreign alike).
pub(crate) fn peek_kind(path: &Path) -> Result<crate::connection::FileKind> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut head = [0u8; 110];
    let mut filled = 0usize;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(SqlError::Io(e)),
        }
    }
    if filled < HEADER_LEN {
        return Err(SqlError::NotSqliteFile("file smaller than SQLite header".into()));
    }
    parse_header(&head[..HEADER_LEN])?;
    if filled >= 106 && head[100..106] == *SQLKIT_MARKER {
        Ok(crate::connection::FileKind::Snapshot)
    } else {
        Ok(crate::connection::FileKind::Native)
    }
}

/// Load tables from a snapshot file. Files without the SQLKit marker are
/// read as foreign SQLite B-Tree files via [`crate::btree`]; files larger
/// than 256 MiB are refused with `Unsupported`.
pub fn load(path: &Path) -> Result<HashMap<String, Table>> {
    validate(path)?;
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let mut marker = [0u8; 6];
    if reader.read_exact(&mut marker).is_err() || &marker != SQLKIT_MARKER {
        drop(reader);
        return crate::btree::load_foreign(path);
    }
    let mut version_bytes = [0u8; 4];
    reader.read_exact(&mut version_bytes)?;
    let _version = u32::from_be_bytes(version_bytes);
    let mut json = Vec::new();
    reader.read_to_end(&mut json)?;
    if json.is_empty() {
        return Ok(HashMap::new());
    }
    let tables: HashMap<String, Table> =
        serde_json::from_slice(&json).map_err(SqlError::Serde)?;
    Ok(tables)
}

/// Persist tables atomically in the LEGACY snapshot format (header + marker
/// + JSON). Kept for migration tests and for readers that still produce
/// snapshots; production writes use the native B-Tree path in
/// [`crate::btree_write::persist_native`].
///
/// Sidecar handling matches the native path: unique temp name, `O_EXCL`
/// creation, owner-only permissions, symlinks refused.
pub fn save(path: &Path, tables: &HashMap<String, Table>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let (tmp, file) = crate::btree_write::create_unique_tmp_for(path)?;
    {
        let mut writer = BufWriter::new(file);
        writer.write_all(&build_header())?;
        writer.write_all(SQLKIT_MARKER)?;
        writer.write_all(&SNAPSHOT_VERSION.to_be_bytes())?;
        serde_json::to_writer(&mut writer, tables).map_err(SqlError::Serde)?;
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| SqlError::Io(e.into_error()))?;
        file.sync_all()?;
    }
    crate::btree_write::refuse_symlink_for(path)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Create a fresh database file as a REAL minimal native SQLite database
/// (100-byte header, empty `sqlite_master` leaf on page 1).
pub fn create_new(path: &Path) -> Result<()> {
    crate::btree_write::create_empty_db(path)?;
    Ok(())
}

/// True when the file at `path` carries the legacy `TSQL01` snapshot marker.
pub fn is_snapshot_file(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    crate::btree_write::is_snapshot_bytes(&bytes)
}
