//! File pager: SQLite header handling plus atomic snapshot persistence.
//!
//! Basis scope: every database file SQLKit creates starts with a real
//! 100-byte SQLite header (`SQLite format 3\0`, page size 4096), followed by
//! the marker `TSQL01` and a JSON snapshot of tables. This keeps files
//! recognizable as SQLite and validates foreign files instead of corrupting
//! them. Foreign SQLite B-Tree files are readable through the milestone 1
//! read path (see [`crate::btree`]); native B-Tree writes are milestone 2
//! work and writes keep persisting the snapshot format below.
//!
//! I/O is streaming-oriented: rows are serialized incrementally and the
//! file is replaced atomically (temp file + rename + fsync), so readers
//! never see a half-written database.

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

/// Persist tables atomically: write header + marker + JSON to a temp file,
/// fsync, then rename over the target.
pub fn save(path: &Path, tables: &HashMap<String, Table>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("tsql-tmp");
    {
        let file = File::create(&tmp)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&build_header())?;
        writer.write_all(SQLKIT_MARKER)?;
        writer.write_all(&SNAPSHOT_VERSION.to_be_bytes())?;
        serde_json::to_writer(&mut writer, tables).map_err(SqlError::Serde)?;
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| SqlError::Io(e.into_error()))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 == 0 {
                perms.set_mode(0o600);
                let _ = fs::set_permissions(path, perms);
            }
        }
    }
    Ok(())
}

/// Create a fresh database file with header + empty snapshot.
pub fn create_new(path: &Path) -> Result<()> {
    save(path, &HashMap::new())
}
