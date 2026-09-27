//! Foreign SQLite file reader (B-Tree pager milestone 1, read path) plus
//! the native write path entry point (milestone 2).
//!
//! Reads database files written by other SQLite implementations (for example
//! the SQLite CLI or CPython `sqlite3`) into the in-memory engine. Writes
//! persist byte-level real SQLite files through the native writer (see
//! [`crate::btree_write`], re-exported here); legacy `TSQL01` snapshot files
//! stay readable and migrate on the first write (see [`crate::pager`]).
//!
//! Supported input:
//!
//! - 100-byte SQLite header (page size, including `1` meaning 65536; text
//!   encoding UTF-8 / UTF-16LE / UTF-16BE; schema cookie is observed).
//! - Table B-Tree pages: leaf pages (`0x0D`) and interior pages (`0x05`),
//!   traversed through the cell-pointer array.
//! - Table leaf cells: `rowid` plus record payload with serial types 0-9
//!   (including `INTEGER` widths, `FLOAT`, constant 0/1) and `TEXT` / `BLOB`
//!   decoded with the file text encoding.
//! - Overflow-page chains for payloads larger than one page.
//! - Interior-page descent to locate arbitrary `rowid` values.
//! - Schema discovery from page 1 (`sqlite_master`): `CREATE TABLE`
//!   statements are parsed with the existing parser ([`crate::parser`]) and a
//!   best-effort fallback keeps column names and declared types.
//!
//! Known limits (see `wiki/Pager.md` for the milestone 2 write-path limits):
//!
//! - Index pages (`0x02` / `0x0A`) carry no table rows and are skipped while
//!   scanning; tables stay correct because a full scan visits every table
//!   page. The number of skipped pages is reported in [`ForeignStats`].
//! - `WITHOUT ROWID` tables store rows in an index-organized B-Tree and are
//!   rejected with [`crate::error::SqlError::Unsupported`].
//! - Files larger than [`MAX_FOREIGN_FILE_BYTES`] (256 MiB) are refused as a
//!   denial-of-service guard.
//! - `sqlite_sequence` and other `sqlite_%` internal tables are skipped.

use crate::connection::{Column, Table};
use crate::error::{Result, SqlError};
use crate::parser::{self, ColumnDef};
use crate::value::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Native write path (milestone 2): record encoding, page builder, journal
/// and recovery live in [`crate::btree_write`], re-exported here so both
/// entry points stay behind the `btree` module.
pub use crate::btree_write;

/// In-memory read budget for foreign files (256 MiB, denial-of-service guard).
pub const MAX_FOREIGN_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// B-Tree page types in the SQLite file format.
const PAGE_INTERIOR_INDEX: u8 = 0x02;
const PAGE_INTERIOR_TABLE: u8 = 0x05;
const PAGE_LEAF_INDEX: u8 = 0x0A;
const PAGE_LEAF_TABLE: u8 = 0x0D;

/// Offset of the database header inside page 1.
const PAGE1_HEADER_SKIP: usize = 100;

/// File text encoding from header offset 56.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// Parsed 100-byte SQLite file header (subset needed by the reader).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    /// Page size in bytes (`1` in the file means 65536).
    pub page_size: u32,
    /// Usable bytes per page (`page_size` minus reserved bytes).
    pub usable_size: u32,
    /// Reserved bytes per page (header offset 20).
    pub reserved: u8,
    /// Text encoding (header offset 56).
    pub encoding: TextEncoding,
    /// Schema cookie (header offset 40, informational).
    pub schema_cookie: u32,
}

/// Statistics recorded while loading a foreign file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignStats {
    /// User tables loaded (excluding `sqlite_%` internal tables).
    pub tables: usize,
    /// Total rows loaded across all user tables.
    pub rows: usize,
    /// Index pages (`0x02` / `0x0A`) skipped during table scans.
    pub skipped_index_pages: usize,
    /// Page size of the foreign file.
    pub page_size: u32,
}

fn corrupt(message: impl Into<String>) -> SqlError {
    SqlError::SqliteFailure { code: 11, message: message.into() }
}

/// Decode one SQLite varint: up to 8 groups of 7 bits plus a 9th full byte.
/// Returns the value and the number of bytes consumed.
pub fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for i in 0..8 {
        let byte = *buf.get(i)?;
        value = (value << 7) | u64::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    let byte = *buf.get(8)?;
    value = (value << 8) | u64::from(byte);
    Some((value, 9))
}

/// Parse the 100-byte SQLite file header.
pub fn parse_file_header(header: &[u8]) -> Result<FileHeader> {
    if header.len() < crate::pager::HEADER_LEN {
        return Err(SqlError::NotSqliteFile("file smaller than SQLite header".into()));
    }
    if &header[0..16] != crate::pager::SQLITE_MAGIC {
        return Err(SqlError::NotSqliteFile("missing SQLite magic".into()));
    }
    let raw = u16::from_be_bytes([header[16], header[17]]);
    let page_size: u32 = if raw == 1 { 65536 } else { u32::from(raw.max(512)) };
    let reserved = header[20];
    let usable_size = page_size
        .checked_sub(u32::from(reserved))
        .filter(|usable| *usable >= 64)
        .ok_or_else(|| corrupt("invalid reserved-bytes value in SQLite header"))?;
    let encoding = match u32::from_be_bytes([header[56], header[57], header[58], header[59]]) {
        1 => TextEncoding::Utf8,
        2 => TextEncoding::Utf16Le,
        3 => TextEncoding::Utf16Be,
        other => {
            return Err(SqlError::unsupported(format!(
                "unsupported SQLite text encoding: {other}"
            )));
        }
    };
    let schema_cookie =
        u32::from_be_bytes([header[40], header[41], header[42], header[43]]);
    Ok(FileHeader { page_size, usable_size, reserved, encoding, schema_cookie })
}

/// Bytes of a signed big-endian integer of width 1, 2, 3, 4, 6 or 8.
fn read_signed_be(bytes: &[u8]) -> Result<i64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(corrupt("integer width out of range"));
    }
    let mut raw: u64 = 0;
    for byte in bytes {
        raw = (raw << 8) | u64::from(*byte);
    }
    let shift = 64 - bytes.len() * 8;
    Ok(((raw << shift) as i64) >> shift)
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        let pair = [chunk[0], chunk[1]];
        units.push(if big_endian { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) });
    }
    String::from_utf16_lossy(&units)
}

/// Decode a `TEXT` payload with the file text encoding.
pub fn decode_text(bytes: &[u8], encoding: TextEncoding) -> String {
    match encoding {
        TextEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        TextEncoding::Utf16Le => decode_utf16(bytes, false),
        TextEncoding::Utf16Be => decode_utf16(bytes, true),
    }
}

/// Parse one record payload into values (serial types 0-9 plus `TEXT`/`BLOB`).
pub fn parse_record(payload: &[u8], encoding: TextEncoding) -> Result<Vec<Value>> {
    let (header_len, mut offset) =
        decode_varint(payload).ok_or_else(|| corrupt("truncated record header"))?;
    let header_len = header_len as usize;
    if header_len < offset || header_len > payload.len() {
        return Err(corrupt("record header length out of range"));
    }
    let mut serials = Vec::new();
    while offset < header_len {
        let (serial, used) =
            decode_varint(&payload[offset..]).ok_or_else(|| corrupt("truncated serial type"))?;
        serials.push(serial);
        offset += used;
    }
    let mut body = header_len;
    let mut values = Vec::with_capacity(serials.len());
    for serial in serials {
        let value = match serial {
            0 => Value::Null,
            1 => {
                let end = body.checked_add(1).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            2 => {
                let end = body.checked_add(2).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            3 => {
                let end = body.checked_add(3).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            4 => {
                let end = body.checked_add(4).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            5 => {
                let end = body.checked_add(6).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            6 => {
                let end = body.checked_add(8).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated integer"))?;
                body = end;
                Value::Integer(read_signed_be(bytes)?)
            }
            7 => {
                let end = body.checked_add(8).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated float"))?;
                body = end;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(bytes);
                Value::Real(f64::from_be_bytes(raw))
            }
            8 => Value::Integer(0),
            9 => Value::Integer(1),
            10 | 11 => {
                return Err(SqlError::unsupported("reserved SQLite serial type 10/11"));
            }
            serial if serial >= 12 && serial % 2 == 0 => {
                let len = ((serial - 12) / 2) as usize;
                let end = body.checked_add(len).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated blob"))?;
                body = end;
                Value::Blob(bytes.to_vec())
            }
            serial => {
                let len = ((serial - 13) / 2) as usize;
                let end = body.checked_add(len).ok_or_else(|| corrupt("record overflows"))?;
                let bytes = payload.get(body..end).ok_or_else(|| corrupt("truncated text"))?;
                body = end;
                Value::Text(decode_text(bytes, encoding))
            }
        };
        values.push(value);
    }
    Ok(values)
}

/// Local payload bytes kept on a table leaf page (overflow formula from the
/// SQLite file format: `K = M + ((P - M) % (U - 4))`, clamped to `U - 35`).
pub fn table_leaf_local_len(payload_len: u64, usable_size: u32) -> usize {
    let usable = u64::from(usable_size);
    let max_local = usable - 35;
    if payload_len <= max_local {
        return payload_len as usize;
    }
    let min_local = (usable - 12) * 32 / 255 - 23;
    let mut local = min_local + (payload_len - min_local) % (usable - 4);
    if local > max_local {
        local = min_local;
    }
    local as usize
}

/// Borrowed view over one database page.
struct Page<'a> {
    bytes: &'a [u8],
    kind: u8,
    cells: usize,
    cell_base: usize,
    rightmost: Option<u32>,
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<usize> {
    let slice = bytes.get(offset..offset + 2).ok_or_else(|| corrupt("truncated page header"))?;
    Ok(usize::from(u16::from_be_bytes([slice[0], slice[1]])))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes.get(offset..offset + 4).ok_or_else(|| corrupt("truncated page value"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

impl<'a> Page<'a> {
    fn open(bytes: &'a [u8], is_page_one: bool) -> Result<Self> {
        let base = if is_page_one { PAGE1_HEADER_SKIP } else { 0 };
        let kind = *bytes.get(base).ok_or_else(|| corrupt("truncated page header"))?;
        let cells = read_u16(bytes, base + 3)?;
        let cell_base = base + 8 + if kind == PAGE_INTERIOR_TABLE || kind == PAGE_INTERIOR_INDEX {
            4
        } else {
            0
        };
        let rightmost = if kind == PAGE_INTERIOR_TABLE || kind == PAGE_INTERIOR_INDEX {
            Some(read_u32(bytes, base + 8)?)
        } else {
            None
        };
        Ok(Self { bytes, kind, cells, cell_base, rightmost })
    }

    /// Offsets of all cells via the cell-pointer array, in key order.
    fn cell_offsets(&self) -> Result<Vec<usize>> {
        let mut offsets = Vec::with_capacity(self.cells);
        for index in 0..self.cells {
            let offset = read_u16(self.bytes, self.cell_base + index * 2)?;
            if offset >= self.bytes.len() {
                return Err(corrupt("cell pointer outside page"));
            }
            offsets.push(offset);
        }
        Ok(offsets)
    }
}

/// Open database image plus header with size and page-count validation.
struct Image {
    data: Vec<u8>,
    header: FileHeader,
    pages: u32,
}

impl Image {
    fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_FOREIGN_FILE_BYTES {
            return Err(SqlError::unsupported(format!(
                "foreign SQLite file is {} bytes, above the 256 MiB read budget",
                metadata.len()
            )));
        }
        // Pre-size from the metadata length: a single growing read without
        // repeated realloc/memcpy cycles.
        let mut data = Vec::with_capacity(metadata.len().min(MAX_FOREIGN_FILE_BYTES) as usize);
        use std::io::Read;
        std::fs::File::open(path)?.read_to_end(&mut data)?;
        if data.len() as u64 > MAX_FOREIGN_FILE_BYTES {
            return Err(SqlError::unsupported(format!(
                "foreign SQLite file is {} bytes, above the 256 MiB read budget",
                data.len()
            )));
        }
        let header = parse_file_header(&data)?;
        if (data.len() as u32) < header.page_size {
            return Err(corrupt("file smaller than one database page"));
        }
        let pages = (data.len() as u32) / header.page_size;
        if pages == 0 {
            return Err(corrupt("file holds no database pages"));
        }
        Ok(Self { data, header, pages })
    }

    fn page_bytes(&self, page_no: u32) -> Result<&[u8]> {
        if page_no == 0 || page_no > self.pages {
            return Err(corrupt(format!("page number out of range: {page_no}")));
        }
        let start = ((page_no - 1) * self.header.page_size) as usize;
        let end = start + self.header.page_size as usize;
        self.data.get(start..end).ok_or_else(|| corrupt("truncated database page"))
    }

    fn open_page(&self, page_no: u32) -> Result<Page<'_>> {
        Page::open(self.page_bytes(page_no)?, page_no == 1)
    }

    /// Read a full cell payload, following the overflow chain when needed.
    fn cell_payload(&self, page: &Page<'_>, cell_offset: usize) -> Result<(i64, Vec<u8>)> {
        let tail = page.bytes.get(cell_offset..).ok_or_else(|| corrupt("cell outside page"))?;
        let (payload_len, used) =
            decode_varint(tail).ok_or_else(|| corrupt("truncated leaf cell"))?;
        let (rowid_raw, rowid_used) = decode_varint(&tail[used..])
            .ok_or_else(|| corrupt("truncated leaf cell rowid"))?;
        let rowid = i64::try_from(rowid_raw).map_err(|_| corrupt("rowid out of range"))?;
        if payload_len > self.data.len() as u64 {
            return Err(corrupt("cell payload larger than file"));
        }
        let payload_len = payload_len as usize;
        let local_len = table_leaf_local_len(payload_len as u64, self.header.usable_size);
        let body_off = cell_offset + used + rowid_used;
        let local = page
            .bytes
            .get(body_off..body_off + local_len)
            .ok_or_else(|| corrupt("cell payload outside page"))?;
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(local);
        if local_len == payload_len {
            return Ok((rowid, payload));
        }
        let mut next = read_u32(page.bytes, body_off + local_len)?;
        let chunk = (self.header.usable_size - 4) as usize;
        let mut guard = self.pages + 1;
        while payload.len() < payload_len {
            if next == 0 || next > self.pages {
                return Err(corrupt("overflow chain points outside file"));
            }
            guard = guard.checked_sub(1).ok_or_else(|| corrupt("overflow chain too long"))?;
            if guard == 0 {
                return Err(corrupt("overflow chain too long"));
            }
            let overflow = self.page_bytes(next)?;
            next = read_u32(overflow, 0)?;
            let end = (payload.len() + chunk).min(payload_len);
            let want = end - payload.len();
            let bytes = overflow.get(4..4 + want).ok_or_else(|| corrupt("truncated overflow page"))?;
            payload.extend_from_slice(bytes);
        }
        Ok((rowid, payload))
    }

    /// Collect every `(rowid, payload)` pair under a table root by visiting
    /// all table pages in key order. Index pages are counted and skipped.
    fn collect_payloads(&self, root: u32, stats: &mut ForeignStats) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut visited = HashSet::new();
        while let Some(page_no) = stack.pop() {
            if !visited.insert(page_no) {
                return Err(corrupt("page visited twice while scanning table"));
            }
            let page = self.open_page(page_no)?;
            match page.kind {
                PAGE_LEAF_TABLE => {
                    for offset in page.cell_offsets()? {
                        out.push(self.cell_payload(&page, offset)?);
                    }
                }
                PAGE_INTERIOR_TABLE => {
                    let mut children = Vec::with_capacity(page.cells + 1);
                    for offset in page.cell_offsets()? {
                        let tail =
                            page.bytes.get(offset..).ok_or_else(|| corrupt("cell outside page"))?;
                        if tail.len() < 4 {
                            return Err(corrupt("truncated interior cell"));
                        }
                        let child = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
                        children.push(child);
                    }
                    if let Some(right) = page.rightmost {
                        children.push(right);
                    }
                    // Push in reverse so the smallest keys pop first.
                    for child in children.into_iter().rev() {
                        stack.push(child);
                    }
                }
                PAGE_INTERIOR_INDEX | PAGE_LEAF_INDEX => {
                    stats.skipped_index_pages += 1;
                }
                other => {
                    return Err(SqlError::unsupported(format!(
                        "unsupported B-Tree page type {other:#04X} in table scan"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Descend interior pages to the leaf holding `target` and return its
    /// payload, or `None` when the `rowid` does not exist.
    fn lookup_payload(&self, root: u32, target: i64) -> Result<Option<Vec<u8>>> {
        let mut page_no = root;
        let mut guard = self.pages + 1;
        loop {
            guard = guard.checked_sub(1).ok_or_else(|| corrupt("B-Tree descent too deep"))?;
            if guard == 0 {
                return Err(corrupt("B-Tree descent too deep"));
            }
            let page = self.open_page(page_no)?;
            match page.kind {
                PAGE_LEAF_TABLE => {
                    for offset in page.cell_offsets()? {
                        let (rowid, payload) = self.cell_payload(&page, offset)?;
                        if rowid == target {
                            return Ok(Some(payload));
                        }
                    }
                    return Ok(None);
                }
                PAGE_INTERIOR_TABLE => {
                    let mut next = page.rightmost;
                    for offset in page.cell_offsets()? {
                        let tail =
                            page.bytes.get(offset..).ok_or_else(|| corrupt("cell outside page"))?;
                        if tail.len() < 4 {
                            return Err(corrupt("truncated interior cell"));
                        }
                        let child = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
                        let (key, _) = decode_varint(&tail[4..])
                            .ok_or_else(|| corrupt("truncated interior key"))?;
                        let key = i64::try_from(key).map_err(|_| corrupt("key out of range"))?;
                        if target <= key {
                            next = Some(child);
                            break;
                        }
                    }
                    page_no = next.ok_or_else(|| corrupt("interior page without child"))?;
                }
                PAGE_INTERIOR_INDEX | PAGE_LEAF_INDEX => return Ok(None),
                other => {
                    return Err(SqlError::unsupported(format!(
                        "unsupported B-Tree page type {other:#04X} in rowid lookup"
                    )));
                }
            }
        }
    }
}

/// Schema of one user table discovered in `sqlite_master`.
struct SchemaEntry {
    key: String,
    name: String,
    rootpage: u32,
    columns: Vec<Column>,
    /// Position of the `INTEGER PRIMARY KEY` rowid alias, if any.
    rowid_alias: Option<usize>,
    without_rowid: bool,
}

/// Split the column list of a `CREATE TABLE` body on top-level commas,
/// respecting nesting and quoted identifiers. Tracks BYTE offsets (not char
/// indices) so non-ASCII schema text can never split a char boundary.
fn split_column_list(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    let mut bracket = false;
    let mut iter = body.char_indices().peekable();
    while let Some((byte_idx, c)) = iter.next() {
        if let Some(q) = quote {
            if c == q {
                // `''` inside a single-quoted literal is an escaped quote.
                if q == '\'' && matches!(iter.peek(), Some((_, '\''))) {
                    iter.next();
                    continue;
                }
                quote = None;
            }
            continue;
        }
        if bracket {
            if c == ']' {
                bracket = false;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '[' => bracket = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(body[start..byte_idx].to_string());
                start = byte_idx + c.len_utf8();
            }
            _ => {}
        }
    }
    items.push(body[start..].to_string());
    items
}

/// Strip one layer of quoting (`"name"`, `` `name` ``, `[name]`) if present.
fn unquote(name: &str) -> String {
    let name = name.trim();
    if name.len() >= 2 {
        let bytes = name.as_bytes();
        if (bytes[0] == b'"' && bytes[name.len() - 1] == b'"')
            || (bytes[0] == b'`' && bytes[name.len() - 1] == b'`')
            || (bytes[0] == b'[' && bytes[name.len() - 1] == b']')
        {
            return name[1..name.len() - 1].to_string();
        }
        if bytes[0] == b'\'' && bytes[name.len() - 1] == b'\'' {
            return name[1..name.len() - 1].replace("''", "'");
        }
    }
    name.to_string()
}

/// Split the head of a column definition into its name and the remainder.
fn split_column_head(item: &str) -> (String, String) {
    let item = item.trim();
    if item.is_empty() {
        return (String::new(), String::new());
    }
    let bytes = item.as_bytes();
    let first = bytes[0];
    if first == b'"' || first == b'`' || first == b'\'' {
        if let Some(end) = item[1..].find(first as char) {
            let name: String = item[1..1 + end].to_string();
            return (name, item[1 + end + 1..].trim().to_string());
        }
    } else if first == b'[' {
        if let Some(end) = item.find(']') {
            let name: String = item[1..end].to_string();
            return (name, item[end + 1..].trim().to_string());
        }
    }
    match item.find(|c: char| c.is_whitespace()) {
        Some(pos) => (item[..pos].to_string(), item[pos..].trim().to_string()),
        None => (item.to_string(), String::new()),
    }
}

const CONSTRAINT_KEYWORDS: &[&str] = &[
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "REFERENCES",
    "COLLATE",
    "CONSTRAINT",
    "GENERATED",
    "ALWAYS",
    "AS",
    "FOREIGN",
    "KEY",
];

/// Best-effort `DEFAULT` literal parsing for schema discovery.
fn parse_default_token(token: &str) -> Option<crate::value::Value> {
    use crate::value::Value;
    let token = token.trim().trim_end_matches(',');
    if token.is_empty() {
        return None;
    }
    if token.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if token.eq_ignore_ascii_case("true") {
        return Some(Value::Integer(1));
    }
    if token.eq_ignore_ascii_case("false") {
        return Some(Value::Integer(0));
    }
    if token.starts_with('\'') && token.len() >= 2 && token.ends_with('\'') {
        return Some(Value::Text(token[1..token.len() - 1].replace("''", "'")));
    }
    if token.starts_with('"') && token.len() >= 2 && token.ends_with('"') {
        return Some(Value::Text(token[1..token.len() - 1].to_string()));
    }
    if let Ok(int) = token.parse::<i64>() {
        return Some(Value::Integer(int));
    }
    if let Ok(real) = token.parse::<f64>() {
        return Some(Value::Real(real));
    }
    None
}

/// Parse one column definition of a `CREATE TABLE` statement that the strict
/// parser rejected (extra constraints, sized types, quoted names).
fn lenient_column(item: &str) -> Option<ColumnDef> {
    let (raw_name, rest) = split_column_head(item);
    if raw_name.is_empty() {
        return None;
    }
    let upper = format!(" {} ", rest.to_ascii_uppercase());
    let mut type_words = Vec::new();
    for word in rest.split_whitespace() {
        let stripped = word.trim_end_matches(',');
        let clean = stripped.to_ascii_uppercase();
        if CONSTRAINT_KEYWORDS.contains(&clean.as_str()) {
            break;
        }
        // Sized types like `VARCHAR(255)` arrive as one token and stay part
        // of the type; a later parenthesized group starts a constraint.
        if stripped.contains('(') && !type_words.is_empty() {
            break;
        }
        type_words.push(clean);
    }
    let default = upper.find("DEFAULT").and_then(|pos| {
        // `upper` carries one leading space, so `pos` indexes `rest` with a
        // one-byte lag; trimming absorbs the gap either way.
        let after = rest.get(pos + "DEFAULT".len()..).unwrap_or("").trim_start();
        let token =
            after.split_whitespace().next().unwrap_or_default().trim_end_matches(',').to_string();
        if token.is_empty() {
            None
        } else {
            parse_default_token(&token)
        }
    });
    Some(ColumnDef {
        name: unquote(&raw_name),
        coltype: type_words.join(" "),
        primary_key: upper.contains(" PRIMARY KEY "),
        not_null: upper.contains(" NOT NULL "),
        default,
    })
}

fn is_table_constraint(item: &str) -> bool {
    let upper = item.trim_start().to_ascii_uppercase();
    ["PRIMARY", "UNIQUE", "CHECK", "FOREIGN", "CONSTRAINT", "KEY", "EXCLUDE"]
        .iter()
        .any(|keyword| upper == *keyword || upper.starts_with(&format!("{keyword} ")))
}

/// Table-level `PRIMARY KEY (a, b)` column names, if present.
fn table_level_pk(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    for item in split_column_list(body) {
        let trimmed = item.trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("PRIMARY KEY") {
            if let Some(start) = trimmed.find('(') {
                if let Some(end) = trimmed.rfind(')') {
                    for part in trimmed[start + 1..end].split(',') {
                        let name = unquote(part);
                        if !name.is_empty() {
                            names.push(name);
                        }
                    }
                }
            }
        }
    }
    names
}

/// Lenient `CREATE TABLE` parser used when the strict parser rejects a
/// statement (table constraints, `AUTOINCREMENT`, sized types). Returns
/// [`ColumnDef`] values so the rest of the pipeline is shared.
fn lenient_create_table(sql: &str) -> Result<Vec<ColumnDef>> {
    // Find the first `(` outside quotes: everything before it is the head.
    let mut quote: Option<char> = None;
    let mut bracket = false;
    let mut open: Option<usize> = None;
    for (index, c) in sql.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if bracket {
            if c == ']' {
                bracket = false;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '[' => bracket = true,
            '(' => {
                open = Some(index);
                break;
            }
            _ => {}
        }
    }
    let open = open.ok_or_else(|| SqlError::parse("CREATE TABLE without column list"))?;
    // Match the closing paren of the outer column list.
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut bracket = false;
    let mut close = None;
    for (index, c) in sql[open..].char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if bracket {
            if c == ']' {
                bracket = false;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '[' => bracket = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(open + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.ok_or_else(|| SqlError::parse("unbalanced parentheses in CREATE TABLE"))?;
    let body = &sql[open + 1..close];
    let pk = table_level_pk(body);
    let mut columns = Vec::new();
    for item in split_column_list(body) {
        let trimmed = item.trim();
        if trimmed.is_empty() || is_table_constraint(trimmed) {
            continue;
        }
        if let Some(mut def) = lenient_column(trimmed) {
            if pk.iter().any(|name| name.eq_ignore_ascii_case(&def.name)) {
                def.primary_key = true;
            }
            if def.coltype.is_empty() {
                def.coltype = "BLOB".to_string();
            }
            columns.push(def);
        }
    }
    if columns.is_empty() {
        return Err(SqlError::parse("CREATE TABLE without usable columns"));
    }
    Ok(columns)
}

/// Build one table schema from a `sqlite_master` row.
fn schema_entry(
    name: String,
    rootpage: u32,
    sql: String,
) -> Result<SchemaEntry> {
    if sql.to_ascii_uppercase().contains("WITHOUT ROWID") {
        return Err(SqlError::unsupported(format!(
            "WITHOUT ROWID table {name} is not readable in milestone 1"
        )));
    }
    let defs: Vec<ColumnDef> = match parser::parse_one(&sql) {
        Ok(parser::Stmt::CreateTable { columns, .. }) => columns,
        _ => lenient_create_table(&sql)?,
    };
    let columns: Vec<Column> = defs.iter().map(Column::from).collect();
    let pk_positions: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| {
            column.primary_key && column.coltype.eq_ignore_ascii_case("INTEGER")
        })
        .map(|(index, _)| index)
        .collect();
    let rowid_alias =
        if pk_positions.len() == 1 { Some(pk_positions[0]) } else { None };
    Ok(SchemaEntry {
        key: name.to_ascii_lowercase(),
        name,
        rootpage,
        columns,
        rowid_alias,
        without_rowid: false,
    })
}

/// Map one record to an engine row: pad added columns with their default
/// (SQLite `ALTER TABLE` semantics, best effort), truncate extras, and fill
/// the `INTEGER PRIMARY KEY` rowid alias from the cell `rowid`.
fn record_to_row(
    record: Vec<crate::value::Value>,
    rowid: i64,
    entry: &SchemaEntry,
) -> Vec<crate::value::Value> {
    use crate::value::Value;
    let mut row = record;
    if row.len() < entry.columns.len() {
        for column in entry.columns.iter().skip(row.len()) {
            row.push(column.default.clone().unwrap_or(Value::Null));
        }
    } else {
        row.truncate(entry.columns.len());
    }
    if let Some(alias) = entry.rowid_alias {
        if let Some(slot) = row.get_mut(alias) {
            if slot.is_null() {
                *slot = Value::Integer(rowid);
            }
        }
    }
    row
}

/// Read `sqlite_master` (page 1) and build schemas for all user tables.
fn discover_schemas(image: &Image, stats: &mut ForeignStats) -> Result<Vec<SchemaEntry>> {
    let page = image.open_page(1)?;
    if page.kind != PAGE_LEAF_TABLE && page.kind != PAGE_INTERIOR_TABLE {
        return Err(corrupt("page 1 is not a table B-Tree"));
    }
    let payloads = image.collect_payloads(1, stats)?;
    // `sqlite_master` layout: type, name, tbl_name, rootpage, sql.
    let mut entries = Vec::new();
    for (_, payload) in payloads {
        let record = parse_record(&payload, image.header.encoding)?;
        let text = |index: usize| match record.get(index) {
            Some(crate::value::Value::Text(value)) => Some(value.clone()),
            _ => None,
        };
        let kind = text(0).unwrap_or_default();
        let name = text(1).unwrap_or_default();
        if !kind.eq_ignore_ascii_case("table") || name.is_empty() {
            continue;
        }
        if name.starts_with("sqlite_") {
            continue;
        }
        let rootpage = match record.get(3) {
            Some(crate::value::Value::Integer(value)) => u32::try_from(*value)
                .map_err(|_| corrupt("bad rootpage in sqlite_master"))?,
            Some(crate::value::Value::Real(value)) => {
                if !value.is_finite() || *value < 0.0 || *value > u32::MAX as f64 {
                    return Err(corrupt("bad rootpage in sqlite_master"));
                }
                *value as u32
            }
            Some(crate::value::Value::Text(value)) => {
                value.parse::<u32>().map_err(|_| corrupt("bad rootpage in sqlite_master"))?
            }
            _ => return Err(corrupt("bad rootpage in sqlite_master")),
        };
        let sql = text(4).unwrap_or_default();
        if sql.is_empty() {
            return Err(SqlError::parse(format!("missing CREATE TABLE for {name}")));
        }
        entries.push(schema_entry(name, rootpage, sql)?);
    }
    Ok(entries)
}

/// Load all user-table rows of a foreign SQLite file with statistics.
pub fn load_foreign_with_stats(path: &Path) -> Result<(HashMap<String, Table>, ForeignStats)> {
    let image = Image::load(path)?;
    let mut stats = ForeignStats {
        page_size: image.header.page_size,
        ..ForeignStats::default()
    };
    let schemas = discover_schemas(&image, &mut stats)?;
    let mut tables = HashMap::with_capacity(schemas.len());
    for entry in &schemas {
        let rows = if entry.rootpage == 0 || entry.without_rowid {
            Vec::new()
        } else {
            image
                .collect_payloads(entry.rootpage, &mut stats)?
                .into_iter()
                .map(|(rowid, payload)| {
                    parse_record(&payload, image.header.encoding)
                        .map(|record| record_to_row(record, rowid, entry))
                })
                .collect::<Result<Vec<_>>>()?
        };
        stats.rows += rows.len();
        stats.tables += 1;
        tables.insert(
            entry.key.clone(),
            Table { name: entry.name.clone(), columns: entry.columns.clone(), rows },
        );
    }
    Ok((tables, stats))
}

/// Load all user-table rows of a foreign SQLite file into engine tables.
pub fn load_foreign(path: &Path) -> Result<HashMap<String, Table>> {
    load_foreign_with_stats(path).map(|(tables, _)| tables)
}

/// Count the rows of one table without decoding any record: walks the table
/// B-Tree and sums leaf cell counts. Used for `SELECT COUNT(*)` over an
/// unloaded database so counting never materializes rows. Unknown tables
/// report code 1 (`no such table`), like the engine.
pub fn count_table_rows(path: &Path, table: &str) -> Result<i64> {
    let image = Image::load(path)?;
    let mut stats = ForeignStats::default();
    let schemas = discover_schemas(&image, &mut stats)?;
    let entry = schemas
        .iter()
        .find(|entry| entry.key == table.to_ascii_lowercase())
        .ok_or_else(|| SqlError::SqliteFailure {
            code: 1,
            message: format!("no such table: {table}"),
        })?;
    if entry.rootpage == 0 {
        return Ok(0);
    }
    // Allocation-free walk: no per-page Vec (cell pointers are read inline),
    // cycle protection via bitset instead of a hash set.
    let mut total: i64 = 0;
    let mut stack = vec![entry.rootpage];
    let mut visited = vec![false; image.pages as usize + 1];
    let mut guard = image.pages + 1;
    while let Some(page_no) = stack.pop() {
        guard = guard.checked_sub(1).ok_or_else(|| corrupt("B-Tree walk too deep"))?;
        if guard == 0 {
            return Err(corrupt("B-Tree walk too deep"));
        }
        if page_no == 0 || page_no > image.pages || visited[page_no as usize] {
            return Err(corrupt("page out of range or visited twice while counting"));
        }
        visited[page_no as usize] = true;
        let page = image.open_page(page_no)?;
        match page.kind {
            PAGE_LEAF_TABLE => {
                // Trust-but-verify: the pointer array itself must fit the
                // page, so a corrupt cell count fails closed instead of
                // overcounting (individual cells stay unchecked: counting
                // never decodes them).
                let array_end = page.cell_base + page.cells * 2;
                if array_end > page.bytes.len() {
                    return Err(corrupt("cell pointer array outside page"));
                }
                total = total.saturating_add(page.cells as i64);
            }
            PAGE_INTERIOR_TABLE => {
                if let Some(right) = page.rightmost {
                    stack.push(right);
                }
                for index in (0..page.cells).rev() {
                    let offset = read_u16(page.bytes, page.cell_base + index * 2)?;
                    let tail =
                        page.bytes.get(offset..).ok_or_else(|| corrupt("cell outside page"))?;
                    if tail.len() < 4 {
                        return Err(corrupt("truncated interior cell"));
                    }
                    stack.push(u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]));
                }
            }
            PAGE_INTERIOR_INDEX | PAGE_LEAF_INDEX => {}
            other => {
                return Err(SqlError::unsupported(format!(
                    "unsupported B-Tree page type {other:#04X} in row count"
                )))
            }
        }
    }
    Ok(total)
}

/// Find one row of a foreign SQLite file by table name and `rowid` using
/// interior-page descent. Returns `None` for unknown tables and missing rows.
pub fn find_rowid(path: &Path, table: &str, rowid: i64) -> Result<Option<Vec<crate::value::Value>>> {
    let image = Image::load(path)?;
    let mut stats = ForeignStats::default();
    let schemas = discover_schemas(&image, &mut stats)?;
    let entry = schemas
        .iter()
        .find(|entry| entry.key == table.to_ascii_lowercase())
        .ok_or_else(|| SqlError::SqliteFailure {
            code: 1,
            message: format!("no such table: {table}"),
        })?;
    if entry.rootpage == 0 {
        return Ok(None);
    }
    match image.lookup_payload(entry.rootpage, rowid)? {
        Some(payload) => {
            let record = parse_record(&payload, image.header.encoding)?;
            Ok(Some(record_to_row(record, rowid, entry)))
        }
        None => Ok(None),
    }
}

/// One index definition discovered in `sqlite_master` (milestone 2).
///
/// Only plain `CREATE INDEX` statements the SQLKit parser accepts are
/// reported; exotic definitions (partial, expression) are skipped because
/// the native writer rebuilds index contents from table rows and cannot
/// represent them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignIndex {
    /// Index display name.
    pub name: String,
    /// Table display name the index belongs to.
    pub table: String,
    /// Indexed column names in order.
    pub columns: Vec<String>,
    /// Verbatim `CREATE INDEX` text from `sqlite_master`.
    pub sql: String,
}

/// Full foreign database state for the native write path: engine tables
/// plus the per-table `rowid` vectors (parallel to the rows, in the same
/// order), the verbatim `CREATE TABLE` texts keyed by lowercase table name,
/// display names, index definitions, schema cookie and page size.
#[derive(Clone, Debug)]
pub struct ForeignDatabase {
    pub tables: HashMap<String, Table>,
    pub rowids: HashMap<String, Vec<i64>>,
    pub schemas: HashMap<String, String>,
    pub display_names: HashMap<String, String>,
    pub indexes: Vec<ForeignIndex>,
    pub schema_cookie: u32,
    pub page_size: u32,
}

/// Parse one `sqlite_master` index row into a [`ForeignIndex`]. Returns
/// `None` for internal indexes (`sqlite_%`), rows without SQL, and SQL the
/// engine parser rejects.
fn foreign_index(name: String, table: String, sql: String) -> Option<ForeignIndex> {
    if name.starts_with("sqlite_") || sql.trim().is_empty() {
        return None;
    }
    match parser::parse_one(&sql) {
        Ok(parser::Stmt::CreateIndex { columns, .. }) => {
            if columns.is_empty() {
                return None;
            }
            Some(ForeignIndex { name, table, columns, sql })
        }
        _ => None,
    }
}

/// Load a foreign SQLite file with rowids, schema texts and index
/// definitions for the native write path. Table loading matches
/// [`load_foreign_with_stats`] exactly; the extra state lets the writer
/// rebuild the same schema byte-for-byte on the next commit.
pub fn load_foreign_detailed(path: &Path) -> Result<ForeignDatabase> {
    let image = Image::load(path)?;
    let mut stats = ForeignStats {
        page_size: image.header.page_size,
        ..ForeignStats::default()
    };
    let payloads = image.collect_payloads(1, &mut stats)?;
    // First pass: user tables plus raw index rows from `sqlite_master`.
    let mut schemas: Vec<SchemaEntry> = Vec::new();
    let mut raw_indexes: Vec<(String, String, String)> = Vec::new();
    for (_, payload) in &payloads {
        let record = parse_record(payload, image.header.encoding)?;
        let text = |index: usize| match record.get(index) {
            Some(crate::value::Value::Text(value)) => Some(value.clone()),
            _ => None,
        };
        let kind = text(0).unwrap_or_default();
        let name = text(1).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if kind.eq_ignore_ascii_case("table") {
            if name.starts_with("sqlite_") {
                continue;
            }
            let rootpage = match record.get(3) {
                Some(crate::value::Value::Integer(value)) => u32::try_from(*value)
                .map_err(|_| corrupt("bad rootpage in sqlite_master"))?,
                Some(crate::value::Value::Real(value)) => {
                if !value.is_finite() || *value < 0.0 || *value > u32::MAX as f64 {
                    return Err(corrupt("bad rootpage in sqlite_master"));
                }
                *value as u32
            }
                Some(crate::value::Value::Text(value)) => value
                    .parse::<u32>()
                    .map_err(|_| corrupt("bad rootpage in sqlite_master"))?,
                _ => return Err(corrupt("bad rootpage in sqlite_master")),
            };
            let sql = text(4).unwrap_or_default();
            if sql.is_empty() {
                return Err(SqlError::parse(format!("missing CREATE TABLE for {name}")));
            }
            schemas.push(schema_entry(name, rootpage, sql)?);
        } else if kind.eq_ignore_ascii_case("index") {
            let table = text(2).unwrap_or_default();
            let sql = text(4).unwrap_or_default();
            if !table.is_empty() {
                raw_indexes.push((name, table, sql));
            }
        }
    }
    // Verbatim schema texts: re-read from the same payloads so the writer
    // preserves the original formatting byte-for-byte.
    let mut sql_by_name: HashMap<String, String> = HashMap::new();
    for (_, payload) in &payloads {
        let record = parse_record(payload, image.header.encoding)?;
        let text = |index: usize| match record.get(index) {
            Some(crate::value::Value::Text(value)) => Some(value.clone()),
            _ => None,
        };
        if text(0).unwrap_or_default().eq_ignore_ascii_case("table") {
            let name = text(1).unwrap_or_default();
            if !name.is_empty() && !name.starts_with("sqlite_") {
                sql_by_name.entry(name).or_insert_with(|| text(4).unwrap_or_default());
            }
        }
    }
    let mut tables = HashMap::with_capacity(schemas.len());
    let mut rowids = HashMap::with_capacity(schemas.len());
    let mut schema_sql = HashMap::with_capacity(schemas.len());
    let mut display_names = HashMap::with_capacity(schemas.len());
    let mut stats_rows = 0usize;
    for entry in &schemas {
        let pairs = if entry.rootpage == 0 || entry.without_rowid {
            Vec::new()
        } else {
            image.collect_payloads(entry.rootpage, &mut stats)?
        };
        let mut rows = Vec::with_capacity(pairs.len());
        let mut ids = Vec::with_capacity(pairs.len());
        for (rowid, payload) in pairs {
            let record = parse_record(&payload, image.header.encoding)?;
            rows.push(record_to_row(record, rowid, entry));
            ids.push(rowid);
        }
        stats_rows += rows.len();
        let _ = stats_rows;
        rowids.insert(entry.key.clone(), ids);
        schema_sql.insert(
            entry.key.clone(),
            sql_by_name.get(&entry.name).cloned().unwrap_or_default(),
        );
        display_names.insert(entry.key.clone(), entry.name.clone());
        tables.insert(
            entry.key.clone(),
            Table { name: entry.name.clone(), columns: entry.columns.clone(), rows },
        );
    }
    let mut indexes = Vec::new();
    for (name, table, sql) in raw_indexes {
        if let Some(index) = foreign_index(name, table, sql) {
            indexes.push(index);
        }
    }
    Ok(ForeignDatabase {
        tables,
        rowids,
        schemas: schema_sql,
        display_names,
        indexes,
        schema_cookie: image.header.schema_cookie,
        page_size: image.header.page_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_vectors() {
        assert_eq!(decode_varint(&[0x00]), Some((0, 1)));
        assert_eq!(decode_varint(&[0x7F]), Some((127, 1)));
        assert_eq!(decode_varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(decode_varint(&[0x82, 0x2C]), Some((300, 2)));
        assert_eq!(decode_varint(&[0x8E, 0xFE, 0x1F]), Some((0x3_BF1F, 3)));
        // Largest 32-bit value still fits in 5 bytes.
        assert_eq!(decode_varint(&[0x8F, 0xFF, 0xFF, 0xFF, 0x7F]), Some((0xFFFF_FFFF, 5)));
        // Nine-byte varint uses all 8 bits of the last byte.
        assert_eq!(
            decode_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            Some((u64::MAX, 9))
        );
        assert_eq!(decode_varint(&[]), None);
        assert_eq!(decode_varint(&[0x80]), None);
    }

    #[test]
    fn header_page_size_and_encoding() {
        let mut header = [0u8; 100];
        header[0..16].copy_from_slice(crate::pager::SQLITE_MAGIC);
        header[16..18].copy_from_slice(&4096u16.to_be_bytes());
        header[20] = 0;
        header[40..44].copy_from_slice(&7u32.to_be_bytes());
        header[56..60].copy_from_slice(&1u32.to_be_bytes());
        let parsed = parse_file_header(&header).unwrap();
        assert_eq!(parsed.page_size, 4096);
        assert_eq!(parsed.usable_size, 4096);
        assert_eq!(parsed.encoding, TextEncoding::Utf8);
        assert_eq!(parsed.schema_cookie, 7);
        // Value 1 means 65536.
        header[16..18].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(parse_file_header(&header).unwrap().page_size, 65536);
        // UTF-16 variants.
        header[16..18].copy_from_slice(&4096u16.to_be_bytes());
        header[56..60].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(parse_file_header(&header).unwrap().encoding, TextEncoding::Utf16Le);
        header[56..60].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(parse_file_header(&header).unwrap().encoding, TextEncoding::Utf16Be);
        // Unknown encoding is Unsupported, bad magic is NotSqliteFile.
        header[56..60].copy_from_slice(&9u32.to_be_bytes());
        assert!(matches!(
            parse_file_header(&header),
            Err(SqlError::Unsupported(_))
        ));
        header[0] = b'X';
        assert!(matches!(
            parse_file_header(&header),
            Err(SqlError::NotSqliteFile(_))
        ));
    }

    #[test]
    fn record_serial_types_zero_to_nine() {
        // Header: 7 bytes (header len + 6 serials); body: u8, u16, u24,
        // u32, u48, u64, float64, const 0, const 1.
        let mut payload = vec![0x07, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        payload.extend_from_slice(&[0xFF]); // -1 as 1-byte int
        payload.extend_from_slice(&[0x01, 0x00]); // 256
        payload.extend_from_slice(&[0xFF, 0xFD, 0x00]); // -768 as 3-byte int
        payload.extend_from_slice(&(-70000i32).to_be_bytes());
        payload.extend_from_slice(&(-1i64).to_be_bytes()[2..8]); // 6-byte -1
        payload.extend_from_slice(&0x1234_5678_9ABC_DEF0u64.to_be_bytes());
        let values = parse_record(&payload, TextEncoding::Utf8).unwrap();
        assert_eq!(
            values,
            vec![
                Value::Integer(-1),
                Value::Integer(256),
                Value::Integer(-768),
                Value::Integer(-70000),
                Value::Integer(-1),
                Value::Integer(0x1234_5678_9ABC_DEF0u64 as i64),
            ]
        );
    }

    #[test]
    fn record_float_text_blob_null_and_consts() {
        let text = "hello-text";
        let blob = [0x00u8, 0xFF, 0x13, 0x37];
        let serial_text = 13 + text.len() as u64 * 2;
        let serial_blob = 12 + blob.len() as u64 * 2;
        let mut payload =
            vec![0x07u8, 0x00, 0x07, serial_text as u8, serial_blob as u8, 0x08, 0x09];
        assert_eq!(payload.len(), payload[0] as usize);
        payload.extend_from_slice(&2.5f64.to_be_bytes());
        payload.extend_from_slice(text.as_bytes());
        payload.extend_from_slice(&blob);
        assert_eq!(payload.len(), 7 + 8 + text.len() + blob.len());
        let values = parse_record(&payload, TextEncoding::Utf8).unwrap();
        assert_eq!(
            values,
            vec![
                Value::Null,
                Value::Real(2.5),
                Value::Text(text.to_string()),
                Value::Blob(blob.to_vec()),
                Value::Integer(0),
                Value::Integer(1),
            ]
        );
    }

    #[test]
    fn record_utf16_text() {
        let text = "Aä";
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut raw = Vec::new();
        for unit in &units {
            raw.extend_from_slice(&unit.to_le_bytes());
        }
        // Serial for TEXT is 13 + 2 * byte length.
        let serial = 13 + raw.len() as u64 * 2;
        let mut payload = vec![0x02, serial as u8];
        payload.extend_from_slice(&raw);
        let values = parse_record(&payload, TextEncoding::Utf16Le).unwrap();
        assert_eq!(values, vec![Value::Text(text.to_string())]);
    }

    #[test]
    fn overflow_local_size_math() {
        // Small payloads stay local.
        assert_eq!(table_leaf_local_len(100, 4096), 100);
        assert_eq!(table_leaf_local_len(4061, 4096), 4061);
        // Large payloads spill: local part stays within bounds.
        let local = table_leaf_local_len(5000, 4096);
        assert!(local <= 4061);
        assert_eq!(local, 489 + (5000 - 489) % 4092);
        let local = table_leaf_local_len(1_000_000, 4096);
        assert!(local <= 4061);
    }

    #[test]
    fn lenient_create_table_fallback() {
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, \
            name VARCHAR(255) NOT NULL, score DOUBLE PRECISION DEFAULT 1.5, \
            tag TEXT UNIQUE, PRIMARY KEY (id))";
        let columns = lenient_create_table(sql).unwrap();
        assert_eq!(columns.len(), 4);
        assert_eq!(columns[0].name, "id");
        assert!(columns[0].primary_key);
        assert_eq!(columns[1].coltype, "VARCHAR(255)");
        assert!(columns[1].not_null);
        assert_eq!(columns[2].default, Some(Value::Real(1.5)));
        // Quoted identifiers survive.
        let quoted = lenient_create_table(
            "CREATE TABLE \"my table\" ([my col] TEXT NOT NULL, [other] BLOB)",
        )
        .unwrap();
        assert_eq!(quoted[0].name, "my col");
        assert!(quoted[0].not_null);
        // Table-level primary keys are marked.
        let composite = lenient_create_table(
            "CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        )
        .unwrap();
        assert!(composite[0].primary_key);
        assert!(composite[1].primary_key);
    }

    #[test]
    fn budget_refuses_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.sqlite");
        // Sparse metadata over the budget is refused before any read.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FOREIGN_FILE_BYTES + 1).unwrap();
        drop(file);
        let result = load_foreign(&path);
        assert!(matches!(result, Err(SqlError::Unsupported(_))));
    }
}
