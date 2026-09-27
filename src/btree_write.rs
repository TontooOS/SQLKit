//! Native SQLite B-Tree write path (milestone 2).
//!
//! This module turns the in-memory tables into a byte-level real SQLite
//! database file: 100-byte header, `sqlite_master` on page 1, table B-Tree
//! pages with record encoding, overflow chains, leaf splits with interior
//! propagation up to the root, index B-Trees for `CREATE INDEX` entries,
//! and freelist trunk handling for pages freed by `DELETE` or rewrites.
//! Every image produced here is readable by other SQLite implementations
//! (rusqlite, the SQLite CLI, CPython `sqlite3`) and files written by them
//! stay readable through the milestone 1 read path in [`crate::btree`].
//!
//! Strategy: each commit rebuilds the whole database image from the current
//! in-memory state and replaces the file atomically (temp file + fsync +
//! rename). Rebuilding keeps every page consistent by construction: cells are
//! packed in key order, overflow chains use the same spill formula as the
//! reader ([`crate::btree::table_leaf_local_len`]), and interior levels are
//! derived bottom-up, including root splits that promote a new interior root.
//! Pages freed by shrinking (for example after `DELETE`) are not truncated
//! away: the previous file size is preserved by linking the surplus pages
//! into the freelist trunk, exactly where SQLite itself would keep them.
//!
//! Crash safety uses rollback-journal mode: the previous file content is
//! copied to `<db>-journal`, fsynced, then the new image is written,
//! fsynced, renamed over the target, and the journal is deleted. On open,
//! [`recover_if_needed`] rolls a leftover journal back before any read.
//! A `-wal` file with content next to the database refuses the open with
//! `Unsupported` ([`check_wal`]) so stale data is never read silently.
//!
//! Known limits (see `wiki/Pager.md`):
//!
//! - The page size of written files is always 4096 with UTF-8 text encoding.
//! - `WITHOUT ROWID` tables stay unsupported on read and are never written.
//! - Index B-Trees cover plain column indexes; exotic index definitions that
//!   the SQLKit parser rejects are kept in memory only and omitted from the
//!   file (data rows stay complete, only the file-side index is missing).
//! - The journal file is SQLKit-format, not SQLite-format: recovery happens
//!   on SQLKit open. A foreign (real SQLite) journal next to the file is
//!   refused with `Unsupported` instead of being applied.

use crate::connection::{Column, Table};
use crate::error::{Result, SqlError};
use crate::value::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

/// Page size of every file written by SQLKit (milestone 2).
pub const NATIVE_PAGE_SIZE: u32 = 4096;
/// Database text encoding of written files: UTF-8.
pub const NATIVE_ENCODING: u32 = 1;
/// Magic prefix of SQLKit rollback-journal files.
const JOURNAL_MAGIC: &[u8; 8] = b"SQLKITJ1";
/// Version of the SQLKit journal layout.
const JOURNAL_VERSION: u32 = 1;
/// Magic prefix of real SQLite rollback journals (first 8 bytes).
const SQLITE_JOURNAL_MAGIC: [u8; 8] = [0xD9, 0xD5, 0x05, 0xF9, 0x20, 0xA1, 0x63, 0xD7];

/// B-Tree page types in the SQLite file format.
const PAGE_INTERIOR_INDEX: u8 = 0x02;
const PAGE_INTERIOR_TABLE: u8 = 0x05;
const PAGE_LEAF_INDEX: u8 = 0x0A;
const PAGE_LEAF_TABLE: u8 = 0x0D;

/// Offset of the database header inside page 1.
const PAGE1_HEADER_SKIP: usize = 100;

// ---------------------------------------------------------------------------
// Crash-simulation hook (used by the journal-recovery test).
// ---------------------------------------------------------------------------

fn crash_slot() -> &'static Mutex<Vec<(String, String)>> {
    static SLOT: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Arm a crash-simulation point for one database path (used by tests). The
/// only production point is `"after_journal"`: the next [`persist_native`]
/// for `path` writes and fsyncs the journal and then fails, leaving the
/// journal behind so a follow-up open must recover it. Pass `None` to disarm
/// the path. Scoped per path so parallel tests never interfere. Never used
/// outside tests.
pub fn set_crash_point_for(path: &Path, point: Option<&str>) {
    let key = path.as_os_str().to_owned();
    let mut slot = crash_slot().lock().unwrap_or_else(|poison| poison.into_inner());
    slot.retain(|(existing, _)| *existing != key.to_string_lossy().into_owned());
    if let Some(point) = point {
        slot.push((key.to_string_lossy().into_owned(), point.to_owned()));
    }
}

fn crash_armed_for(path: &Path, point: &str) -> bool {
    let key = path.as_os_str().to_string_lossy();
    crash_slot()
        .lock()
        .map(|slot| slot.iter().any(|(p, q)| p == &key && q == point))
        .unwrap_or(false)
}

fn crash_here(path: &Path, point: &str) -> Result<()> {
    if crash_armed_for(path, point) {
        // Disarm so the recovery open is not poisoned as well.
        set_crash_point_for(path, None);
        return Err(SqlError::Custom(format!("sqlkit: simulated crash {point}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Varint + record encoding (inverse of `crate::btree` decoding).
// ---------------------------------------------------------------------------

fn varint_len(value: u64) -> usize {
    if value <= 127 {
        1
    } else if value <= 16_383 {
        2
    } else if value <= 2_097_151 {
        3
    } else if value <= 268_435_455 {
        4
    } else if value <= 34_359_738_367 {
        5
    } else if value <= 4_398_046_511_104 {
        6
    } else if value <= 562_949_953_421_311 {
        7
    } else if value <= 72_057_594_037_927_935 {
        8
    } else {
        9
    }
}

/// Encode one SQLite varint (minimal length; 9-byte form carries 8 bits in
/// the last byte). Round-trips through [`crate::btree::decode_varint`].
pub fn encode_varint(value: u64) -> Vec<u8> {
    let len = varint_len(value);
    let mut out = vec![0u8; len];
    if len == 9 {
        for i in 0..8 {
            let shift = 8 + 7 * (7 - i);
            out[i] = 0x80 | ((value >> shift) & 0x7F) as u8;
        }
        out[8] = (value & 0xFF) as u8;
    } else {
        for i in 0..len {
            let shift = 7 * (len - 1 - i);
            let mut byte = ((value >> shift) & 0x7F) as u8;
            if i + 1 < len {
                byte |= 0x80;
            }
            out[i] = byte;
        }
    }
    out
}

/// Minimal serial type for an `INTEGER` value (0/1 use constants 8/9).
fn integer_serial(value: i64) -> u64 {
    if value == 0 {
        8
    } else if value == 1 {
        9
    } else if (-128..=127).contains(&value) {
        1
    } else if (-32_768..=32_767).contains(&value) {
        2
    } else if (-8_388_608..=8_388_607).contains(&value) {
        3
    } else if (-2_147_483_648..=2_147_483_647).contains(&value) {
        4
    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&value) {
        5
    } else {
        6
    }
}

fn integer_width(serial: u64) -> usize {
    match serial {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 => 8,
        _ => 0,
    }
}

fn serial_of(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Integer(v) => integer_serial(*v),
        Value::Real(_) => 7,
        Value::Text(s) => 13 + 2 * s.as_bytes().len() as u64,
        Value::Blob(b) => 12 + 2 * b.len() as u64,
    }
}

fn append_value_bytes(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => {}
        Value::Integer(v) => {
            let serial = integer_serial(*v);
            if serial == 8 || serial == 9 {
                return;
            }
            let width = integer_width(serial);
            let bytes = v.to_be_bytes();
            out.extend_from_slice(&bytes[8 - width..]);
        }
        Value::Real(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Text(s) => out.extend_from_slice(s.as_bytes()),
        Value::Blob(b) => out.extend_from_slice(b),
    }
}

/// Encode one record payload (header varints plus body). Inverse of
/// [`crate::btree::parse_record`] for serial types 0-9 plus `TEXT`/`BLOB`.
pub fn encode_record(values: &[Value]) -> Vec<u8> {
    let serials: Vec<u64> = values.iter().map(serial_of).collect();
    let mut body_len: usize = 0;
    let mut serial_bytes = Vec::with_capacity(serials.len() * 2);
    for (serial, value) in serials.iter().zip(values.iter()) {
        serial_bytes.extend_from_slice(&encode_varint(*serial));
        body_len += match value {
            Value::Null => 0,
            Value::Integer(v) => {
                let serial = integer_serial(*v);
                if serial == 8 || serial == 9 {
                    0
                } else {
                    integer_width(serial)
                }
            }
            Value::Real(_) => 8,
            Value::Text(s) => s.as_bytes().len(),
            Value::Blob(b) => b.len(),
        };
    }
    // The header length counts its own varint; sizes below 128 need one byte
    // and wider rows converge after at most one extra pass.
    let mut header_len = serial_bytes.len() + 1;
    loop {
        let own = varint_len(header_len as u64);
        let total = serial_bytes.len() + own;
        if total == header_len {
            break;
        }
        header_len = total;
    }
    let mut out = Vec::with_capacity(header_len + body_len);
    out.extend_from_slice(&encode_varint(header_len as u64));
    out.extend_from_slice(&serial_bytes);
    for value in values {
        append_value_bytes(&mut out, value);
    }
    debug_assert_eq!(out.len(), header_len + body_len);
    out
}

// ---------------------------------------------------------------------------
// Schema SQL rendering.
// ---------------------------------------------------------------------------

/// Quote one identifier with double quotes (SQLite quoting rules).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn default_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Integer(v) => v.to_string(),
        Value::Real(v) => {
            if v.fract() == 0.0 && v.is_finite() {
                format!("{v:.1}")
            } else {
                format!("{v}")
            }
        }
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            let mut hex = String::with_capacity(b.len() * 2);
            for byte in b {
                hex.push_str(&format!("{byte:02X}"));
            }
            format!("X'{hex}'")
        }
    }
}

/// Render a canonical `CREATE TABLE` statement from engine columns. Tables
/// with a composite primary key use a table-level clause so the statement
/// stays valid SQLite; single-column keys stay inline.
pub fn create_table_sql(table: &str, columns: &[Column]) -> String {
    let pk: Vec<usize> =
        columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(i, _)| i).collect();
    let composite = pk.len() > 1;
    let mut parts = Vec::with_capacity(columns.len() + 1);
    for (index, column) in columns.iter().enumerate() {
        let coltype = if column.coltype.is_empty() { "BLOB".to_owned() } else { column.coltype.clone() };
        let mut def = format!("{} {coltype}", quote_ident(&column.name));
        if column.primary_key && !composite {
            def.push_str(" PRIMARY KEY");
        }
        if column.not_null {
            def.push_str(" NOT NULL");
        }
        if let Some(default) = &column.default {
            def.push_str(&format!(" DEFAULT {}", default_literal(default)));
        }
        let _ = index;
        parts.push(def);
    }
    if composite {
        let names: Vec<String> =
            pk.iter().map(|i| quote_ident(&columns[*i].name)).collect();
        parts.push(format!("PRIMARY KEY({})", names.join(", ")));
    }
    format!("CREATE TABLE {} ({})", quote_ident(table), parts.join(", "))
}

/// Render a canonical `CREATE INDEX` statement.
pub fn create_index_sql(name: &str, table: &str, columns: &[String]) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    format!("CREATE INDEX {} ON {} ({})", quote_ident(name), quote_ident(table), cols.join(", "))
}

// ---------------------------------------------------------------------------
// Overflow math for index pages.
// ---------------------------------------------------------------------------

/// Local payload bytes kept on an index page. SQLite uses
/// `X = ((U-12)*64/255)-23` for index pages (table leaves use `U-35`).
pub fn index_leaf_local_len(payload_len: u64, usable_size: u32) -> usize {
    let usable = u64::from(usable_size);
    let max_local = (usable - 12) * 64 / 255 - 23;
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

// ---------------------------------------------------------------------------
// Page allocator + freelist.
// ---------------------------------------------------------------------------

/// In-memory freelist trunk: freed pages linked as SQLite trunk pages
/// (`next trunk`, `leaf count`, leaf page numbers). exercised by unit tests
/// and used to preserve the previous file size on shrink.
#[derive(Clone, Debug, Default)]
pub struct FreelistChain {
    /// Trunk pages in file order: each entry holds the leaf pages attached
    /// to that trunk.
    trunks: Vec<(u32, Vec<u32>)>,
}

impl FreelistChain {
    pub fn new() -> Self {
        Self { trunks: Vec::new() }
    }

    /// Push one freed page: it becomes a leaf of the current trunk, or a
    /// new trunk when the current one is full. A trunk page holds
    /// `(usable - 8) / 4` leaf entries.
    pub fn push(&mut self, pgno: u32, usable: u32) {
        let capacity = ((usable - 8) / 4) as usize;
        let capacity = capacity.max(1);
        match self.trunks.last_mut() {
            Some((_, leaves)) if leaves.len() < capacity => leaves.push(pgno),
            _ => self.trunks.push((pgno, Vec::new())),
        }
    }

    /// Build the chain from an ordered page list (used when the rebuilt
    /// image is smaller than the previous file).
    pub fn from_pages(pages: &[u32], usable: u32) -> Self {
        let mut chain = Self::new();
        for pgno in pages {
            chain.push(*pgno, usable);
        }
        chain
    }

    pub fn trunk_count(&self) -> usize {
        self.trunks.len()
    }

    /// Total free pages including trunk pages themselves.
    pub fn total(&self) -> u32 {
        self.trunks.iter().map(|(_, leaves)| leaves.len() as u32 + 1).sum()
    }

    pub fn first_trunk(&self) -> u32 {
        self.trunks.first().map(|(pgno, _)| *pgno).unwrap_or(0)
    }

    /// Render trunk pages into the image (leaf pages stay zeroed).
    pub fn render_into(&self, image: &mut [u8], page_size: u32) {
        for (index, (trunk, leaves)) in self.trunks.iter().enumerate() {
            let next = self.trunks.get(index + 1).map(|(pgno, _)| *pgno).unwrap_or(0);
            let start = (*trunk - 1) * page_size;
            let page = &mut image[start as usize..(start + page_size) as usize];
            page[0..4].copy_from_slice(&next.to_be_bytes());
            page[4..8].copy_from_slice(&(leaves.len() as u32).to_be_bytes());
            for (slot, leaf) in leaves.iter().enumerate() {
                let off = 8 + slot * 4;
                page[off..off + 4].copy_from_slice(&leaf.to_be_bytes());
            }
        }
    }
}

/// Sequential page allocator over the rebuilt image. Fresh rebuilds always
/// append (freelist is empty by construction); freed surplus pages of the
/// previous file are re-attached as a freelist trunk afterwards.
struct Allocator {
    next: u32,
    page_size: u32,
    pages: HashMap<u32, Vec<u8>>,
}

impl Allocator {
    fn new(page_size: u32) -> Self {
        Self { next: 2, page_size, pages: HashMap::new() }
    }

    fn alloc(&mut self) -> u32 {
        let pgno = self.next;
        self.next += 1;
        pgno
    }

    fn put(&mut self, pgno: u32, bytes: Vec<u8>) {
        assert_eq!(bytes.len(), self.page_size as usize, "page image has wrong size");
        self.pages.insert(pgno, bytes);
    }

    fn page_count(&self) -> u32 {
        self.next - 1
    }
}

// ---------------------------------------------------------------------------
// Page rendering.
// ---------------------------------------------------------------------------

fn render_page(
    kind: u8,
    cells: &[Vec<u8>],
    rightmost: Option<u32>,
    page_size: u32,
    base: usize,
) -> Vec<u8> {
    let header_len = if rightmost.is_some() { 12 } else { 8 };
    let total_cells: usize = cells.iter().map(Vec::len).sum();
    let ptrs = cells.len() * 2;
    let content_start = page_size as usize - total_cells;
    debug_assert!(base + header_len + ptrs <= content_start, "page overfull");
    let mut page = vec![0u8; page_size as usize];
    page[base] = kind;
    let count = cells.len() as u16;
    page[base + 3..base + 5].copy_from_slice(&count.to_be_bytes());
    page[base + 5..base + 7].copy_from_slice(&(content_start as u16).to_be_bytes());
    if let Some(right) = rightmost {
        page[base + 8..base + 12].copy_from_slice(&right.to_be_bytes());
    }
    let mut cursor = content_start;
    for (index, cell) in cells.iter().enumerate() {
        let off = base + header_len + index * 2;
        // Cell offsets are absolute page offsets (they include the page-1
        // header skip because `cursor` counts from the page start).
        let absolute = cursor;
        page[off..off + 2].copy_from_slice(&(absolute as u16).to_be_bytes());
        page[cursor..cursor + cell.len()].copy_from_slice(cell);
        cursor += cell.len();
    }
    page
}

/// Split one payload across overflow pages. Returns the first overflow page
/// number (0 when everything stays local) and registers the overflow pages.
fn spill_payload(
    alloc: &mut Allocator,
    payload: &[u8],
    local_len: usize,
    usable: u32,
) -> (u32, Vec<u8>) {
    if local_len >= payload.len() {
        return (0, payload.to_vec());
    }
    let chunk = (usable - 4) as usize;
    let rest = &payload[local_len..];
    let count = rest.len().div_ceil(chunk);
    let mut pgnos = Vec::with_capacity(count);
    for _ in 0..count {
        pgnos.push(alloc.alloc());
    }
    for (i, pgno) in pgnos.iter().enumerate() {
        let next = pgnos.get(i + 1).copied().unwrap_or(0);
        let start = i * chunk;
        let end = ((i + 1) * chunk).min(rest.len());
        let mut page = vec![0u8; alloc.page_size as usize];
        page[0..4].copy_from_slice(&next.to_be_bytes());
        page[4..4 + (end - start)].copy_from_slice(&rest[start..end]);
        alloc.put(*pgno, page);
    }
    (pgnos[0], payload[..local_len].to_vec())
}

fn table_leaf_cell(rowid: i64, payload: &[u8], alloc: &mut Allocator, usable: u32) -> Vec<u8> {
    let local = crate::btree::table_leaf_local_len(payload.len() as u64, usable);
    let (first, local_bytes) = spill_payload(alloc, payload, local, usable);
    let mut cell = Vec::new();
    cell.extend_from_slice(&encode_varint(payload.len() as u64));
    cell.extend_from_slice(&encode_varint(rowid as u64));
    cell.extend_from_slice(&local_bytes);
    if first != 0 {
        cell.extend_from_slice(&first.to_be_bytes());
    }
    cell
}

fn index_leaf_cell(payload: &[u8], alloc: &mut Allocator, usable: u32) -> Vec<u8> {
    let local = index_leaf_local_len(payload.len() as u64, usable);
    let (first, local_bytes) = spill_payload(alloc, payload, local, usable);
    let mut cell = Vec::new();
    cell.extend_from_slice(&encode_varint(payload.len() as u64));
    cell.extend_from_slice(&local_bytes);
    if first != 0 {
        cell.extend_from_slice(&first.to_be_bytes());
    }
    cell
}

fn index_interior_cell(child: u32, payload: &[u8], alloc: &mut Allocator, usable: u32) -> Vec<u8> {
    let local = index_leaf_local_len(payload.len() as u64, usable);
    let (first, local_bytes) = spill_payload(alloc, payload, local, usable);
    let mut cell = Vec::new();
    cell.extend_from_slice(&child.to_be_bytes());
    cell.extend_from_slice(&encode_varint(payload.len() as u64));
    cell.extend_from_slice(&local_bytes);
    if first != 0 {
        cell.extend_from_slice(&first.to_be_bytes());
    }
    cell
}

/// Greedy cell packing: fill each page while header, pointer array and cell
/// bytes fit; every page carries at least one cell.
fn pack_cells(sizes: &[usize], capacity: usize, header: usize) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut current = 0usize;
    let mut used = 0usize;
    for size in sizes {
        let trial = if current == 0 { header + 2 + size } else { used + 2 + size };
        if current > 0 && trial > capacity {
            counts.push(current);
            current = 0;
            used = 0;
        }
        if current == 0 {
            used = header + 2 + size;
        } else {
            used += 2 + size;
        }
        current += 1;
    }
    if current > 0 {
        counts.push(current);
    }
    counts
}

/// Build a table B-Tree bottom-up: leaf split by capacity, then interior
/// levels until one root remains. Returns the root page number.
fn build_table_btree(
    alloc: &mut Allocator,
    entries: &[(i64, Vec<u8>)],
    page_size: u32,
    usable: u32,
) -> Result<u32> {
    if entries.is_empty() {
        let pgno = alloc.alloc();
        alloc.put(pgno, render_page(PAGE_LEAF_TABLE, &[], None, page_size, 0));
        return Ok(pgno);
    }
    let mut cells = Vec::with_capacity(entries.len());
    for (rowid, payload) in entries {
        cells.push(table_leaf_cell(*rowid, payload, alloc, usable));
    }
    let capacity = page_size as usize;
    let sizes: Vec<usize> = cells.iter().map(Vec::len).collect();
    let mut level: Vec<(u32, i64)> = Vec::new();
    let mut offset = 0usize;
    for count in pack_cells(&sizes, capacity, 8) {
        let group = &cells[offset..offset + count];
        let max_rowid = entries[offset + count - 1].0;
        let pgno = alloc.alloc();
        alloc.put(pgno, render_page(PAGE_LEAF_TABLE, group, None, page_size, 0));
        level.push((pgno, max_rowid));
        offset += count;
    }
    // Interior propagation: each parent cell carries the child page plus the
    // maximum rowid below it; the last child of a group becomes the
    // rightmost pointer (which costs no cell bytes, so a trailing single
    // child always merges into the previous group for free).
    while level.len() > 1 {
        let mut next_level = Vec::new();
        let mut start = 0usize;
        while start < level.len() {
            // Grow the group while divider cells fit. A group needs at least
            // two children (one divider plus the rightmost one).
            let mut end = start + 2;
            if end > level.len() {
                end = level.len();
            }
            loop {
                if end >= level.len() {
                    break;
                }
                // Extending the group by one child turns the current
                // rightmost child into a divider cell.
                let divider_key = level[end - 1].1;
                let cost = 2 + 4 + varint_len(divider_key as u64);
                let used: usize = group_byte_size(&level[start..end]);
                if used + cost > capacity {
                    break;
                }
                end += 1;
            }
            // A trailing single child merges into this group for free: the
            // rightmost pointer lives in the page header, so adopting one
            // more rightmost child costs zero bytes.
            if end < level.len() && end + 1 == level.len() && end > start + 1 {
                end += 1;
            }
            let mut parent_cells = Vec::new();
            for slot in start..end - 1 {
                let mut cell = Vec::new();
                cell.extend_from_slice(&level[slot].0.to_be_bytes());
                cell.extend_from_slice(&encode_varint(level[slot].1 as u64));
                parent_cells.push(cell);
            }
            let rightmost = level[end - 1].0;
            let max_key = level[end - 1].1;
            let pgno = alloc.alloc();
            alloc.put(
                pgno,
                render_page(PAGE_INTERIOR_TABLE, &parent_cells, Some(rightmost), page_size, 0),
            );
            next_level.push((pgno, max_key));
            start = end;
        }
        level = next_level;
    }
    Ok(level[0].0)
}

/// Byte size of one interior-table parent group: 12-byte header, pointer
/// array and divider cells for every child but the rightmost one.
fn group_byte_size(children: &[(u32, i64)]) -> usize {
    let mut size = 12usize;
    for (index, (_, key)) in children.iter().enumerate() {
        if index + 1 < children.len() {
            size += 2 + 4 + varint_len(*key as u64);
        }
    }
    size
}

/// Build an index B-Tree bottom-up over `(key record, rowid)` entries that
/// are already sorted. Returns the root page number.
fn build_index_btree(
    alloc: &mut Allocator,
    entries: &[Vec<u8>],
    page_size: u32,
    usable: u32,
) -> Result<u32> {
    if entries.is_empty() {
        let pgno = alloc.alloc();
        alloc.put(pgno, render_page(PAGE_LEAF_INDEX, &[], None, page_size, 0));
        return Ok(pgno);
    }
    let mut cells = Vec::with_capacity(entries.len());
    for payload in entries {
        cells.push(index_leaf_cell(payload, alloc, usable));
    }
    let capacity = page_size as usize;
    let sizes: Vec<usize> = cells.iter().map(Vec::len).collect();
    // Level entries: (page number, divider payload = max key of the subtree).
    let mut level: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut offset = 0usize;
    for count in pack_cells(&sizes, capacity, 8) {
        let group = &cells[offset..offset + count];
        let divider = entries[offset + count - 1].clone();
        let pgno = alloc.alloc();
        alloc.put(pgno, render_page(PAGE_LEAF_INDEX, group, None, page_size, 0));
        level.push((pgno, divider));
        offset += count;
    }
    while level.len() > 1 {
        let mut next_level: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut index = 0usize;
        while index < level.len() {
            // Pack divider cells greedily; the last child of each group is
            // the rightmost pointer.
            let mut end = index + 1;
            let mut used = 12usize;
            while end < level.len() {
                let probe = index_interior_cell_size(&level[end - 1].1);
                if used + 2 + probe > capacity {
                    break;
                }
                used += 2 + probe;
                end += 1;
                if used + 6 > capacity {
                    break;
                }
            }
            if end == index + 1 && end < level.len() {
                // Single divider did not fit the probe above only when the
                // page is tiny; still make progress with one divider.
                end = index + 2;
            }
            let mut parent_cells = Vec::new();
            for slot in index..end - 1 {
                let (child, payload) = &level[slot];
                parent_cells.push(index_interior_cell(*child, payload, alloc, usable));
            }
            let rightmost = level[end - 1].0;
            let divider = level[end - 1].1.clone();
            let pgno = alloc.alloc();
            alloc.put(
                pgno,
                render_page(PAGE_INTERIOR_INDEX, &parent_cells, Some(rightmost), page_size, 0),
            );
            next_level.push((pgno, divider));
            index = end;
        }
        level = next_level;
    }
    Ok(level[0].0)
}

fn index_interior_cell_size(payload: &[u8]) -> usize {
    4 + varint_len(payload.len() as u64) + payload.len()
}

// ---------------------------------------------------------------------------
// Database image assembly.
// ---------------------------------------------------------------------------

/// One table to write: display name, `sqlite_master` SQL, rows with rowids,
/// and the position of the `INTEGER PRIMARY KEY` rowid alias (if any).
pub struct TableInput {
    pub name: String,
    pub sql: String,
    pub rows: Vec<(i64, Vec<Value>)>,
    pub rowid_alias: Option<usize>,
}

/// One index to write: definition plus resolved key values per row.
pub struct IndexInput {
    pub name: String,
    pub sql: String,
    pub table_key: String,
    /// Key values per row in `(key columns..., rowid)` record order.
    pub entries: Vec<Vec<Value>>,
}

/// Options for one database image build.
pub struct BuildOptions {
    pub tables: Vec<TableInput>,
    pub indexes: Vec<IndexInput>,
    pub schema_cookie: u32,
    pub change_counter: u32,
    /// Previous page count (4096-byte pages) used to preserve file size by
    /// linking surplus pages into the freelist trunk. Zero disables padding.
    pub old_page_count: u32,
}

fn write_header(
    header: &mut [u8],
    page_size: u32,
    size_pages: u32,
    freelist_trunk: u32,
    freelist_total: u32,
    schema_cookie: u32,
    change_counter: u32,
) {
    header[0..16].copy_from_slice(crate::pager::SQLITE_MAGIC);
    let raw_size: u16 = if page_size == 65536 { 1 } else { page_size as u16 };
    header[16..18].copy_from_slice(&raw_size.to_be_bytes());
    header[18] = 1;
    header[19] = 1;
    header[20] = 0;
    header[21] = 64;
    header[22] = 32;
    header[23] = 32;
    header[24..28].copy_from_slice(&change_counter.to_be_bytes());
    header[28..32].copy_from_slice(&size_pages.to_be_bytes());
    header[32..36].copy_from_slice(&freelist_trunk.to_be_bytes());
    header[36..40].copy_from_slice(&freelist_total.to_be_bytes());
    header[40..44].copy_from_slice(&schema_cookie.to_be_bytes());
    header[44..48].copy_from_slice(&4u32.to_be_bytes());
    header[48..52].copy_from_slice(&2000u32.to_be_bytes());
    header[52..56].copy_from_slice(&0u32.to_be_bytes());
    header[56..60].copy_from_slice(&NATIVE_ENCODING.to_be_bytes());
    header[60..64].copy_from_slice(&0u32.to_be_bytes());
    header[64..68].copy_from_slice(&0u32.to_be_bytes());
    header[68..72].copy_from_slice(&0u32.to_be_bytes());
    for byte in header.iter_mut().take(92).skip(72) {
        *byte = 0;
    }
    header[92..96].copy_from_slice(&change_counter.to_be_bytes());
    header[96..100].copy_from_slice(&3_047_000u32.to_be_bytes());
}

/// Assemble a complete database image. Tables are laid out first (page 2+),
/// then indexes, then the `sqlite_master` content whose root is page 1.
pub fn build_database_image(options: BuildOptions) -> Result<Vec<u8>> {
    let page_size = NATIVE_PAGE_SIZE;
    let usable = page_size;
    let mut alloc = Allocator::new(page_size);

    // Sort tables by lowercase name for a deterministic layout.
    let mut tables = options.tables;
    tables.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    let mut indexes = options.indexes;
    indexes.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));

    // Phase 1: user tables (rowids sorted ascending, alias stored as NULL).
    struct PlacedTable {
        name: String,
        sql: String,
        root: u32,
    }
    let mut placed_tables = Vec::with_capacity(tables.len());
    for table in &tables {
        let mut rows = table.rows.clone();
        rows.sort_by_key(|(rowid, _)| *rowid);
        let mut entries = Vec::with_capacity(rows.len());
        for (rowid, values) in &rows {
            let mut stored = values.clone();
            if let Some(alias) = table.rowid_alias {
                if let Some(slot) = stored.get_mut(alias) {
                    *slot = Value::Null;
                }
            }
            entries.push((*rowid, encode_record(&stored)));
        }
        let root = build_table_btree(&mut alloc, &entries, page_size, usable)?;
        placed_tables.push(PlacedTable {
            name: table.name.to_owned(),
            sql: table.sql.to_owned(),
            root,
        });
    }

    // Phase 2: index B-Trees. Key records append the rowid so every entry is
    // unique; entries are sorted with the engine ordering plus rowid.
    struct PlacedIndex {
        name: String,
        table: String,
        sql: String,
        root: u32,
    }
    let mut placed_indexes = Vec::with_capacity(indexes.len());
    for index in &indexes {
        let mut keyed: Vec<(Vec<Value>, Vec<u8>)> = Vec::with_capacity(index.entries.len());
        for key in &index.entries {
            keyed.push((key.clone(), encode_record(key)));
        }
        keyed.sort_by(|a, b| {
            let mut ord = std::cmp::Ordering::Equal;
            for (x, y) in a.0.iter().zip(b.0.iter()) {
                ord = crate::parser::sort_compare(x, y);
                if ord != std::cmp::Ordering::Equal {
                    break;
                }
            }
            ord
        });
        let payloads: Vec<Vec<u8>> = keyed.into_iter().map(|(_, payload)| payload).collect();
        let root = build_index_btree(&mut alloc, &payloads, page_size, usable)?;
        placed_indexes.push(PlacedIndex {
            name: index.name.to_owned(),
            table: index.table_key.to_owned(),
            sql: index.sql.to_owned(),
            root,
        });
    }

    // Phase 3: `sqlite_master` rows (tables first, then indexes). Real
    // SQLite files never list `sqlite_master` itself, so no self row.
    let mut master: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut next_rowid: i64 = 1;
    // Master rows reference tables in the same sorted order as the layout.
    let mut table_roots: Vec<(String, String, u32)> = placed_tables
        .iter()
        .map(|t| (t.name.clone(), t.sql.clone(), t.root))
        .collect();
    table_roots.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    for (name, sql, root) in &table_roots {
        master.push((
            next_rowid,
            vec![
                Value::Text("table".to_owned()),
                Value::Text(name.clone()),
                Value::Text(name.clone()),
                Value::Integer(*root as i64),
                Value::Text(sql.clone()),
            ],
        ));
        next_rowid += 1;
    }
    let mut index_roots: Vec<(String, String, String, u32)> = placed_indexes
        .iter()
        .map(|i| (i.name.clone(), i.table.clone(), i.sql.clone(), i.root))
        .collect();
    index_roots.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    for (name, table, sql, root) in &index_roots {
        master.push((
            next_rowid,
            vec![
                Value::Text("index".to_owned()),
                Value::Text(name.clone()),
                Value::Text(table.clone()),
                Value::Integer(*root as i64),
                Value::Text(sql.clone()),
            ],
        ));
        next_rowid += 1;
    }
    let master_entries: Vec<(i64, Vec<u8>)> =
        master.iter().map(|(rowid, values)| (*rowid, encode_record(values))).collect();

    // Master leaves/interiors take fresh pages; the root content lands on
    // page 1 afterwards (page 1 is reserved by `Allocator::new`).
    let master_root = build_master_pages(&mut alloc, &master_entries, page_size, usable)?;
    let _ = master_root;

    let mut new_pages = alloc.page_count().max(1);
    // Preserve the previous file size: surplus pages become freelist trunk.
    let mut freelist = FreelistChain::new();
    if options.old_page_count > new_pages {
        let surplus: Vec<u32> = (new_pages + 1..=options.old_page_count).collect();
        freelist = FreelistChain::from_pages(&surplus, usable);
        new_pages = options.old_page_count;
    }

    let mut image = vec![0u8; new_pages as usize * page_size as usize];
    for (pgno, bytes) in &alloc.pages {
        if *pgno == 1 {
            // Page 1 carries the database header in bytes 0..100; the
            // rendered root leaves that area zeroed, so only the B-Tree
            // bytes are copied here and the header is written afterwards.
            image[PAGE1_HEADER_SKIP..page_size as usize]
                .copy_from_slice(&bytes[PAGE1_HEADER_SKIP..]);
            continue;
        }
        let start = (*pgno - 1) * page_size;
        image[start as usize..(start + page_size) as usize].copy_from_slice(bytes);
    }
    write_header(
        &mut image[0..100],
        page_size,
        new_pages,
        freelist.first_trunk(),
        freelist.total(),
        options.schema_cookie,
        options.change_counter,
    );
    insert_master_root(&mut image, &alloc, page_size)?;
    freelist.render_into(&mut image, page_size);
    Ok(image)
}

/// Page-1 scratch key: `build_master_pages` stashes the rendered root page
/// (without the 100-byte header) here for [`insert_master_root`].
fn build_master_pages(
    alloc: &mut Allocator,
    entries: &[(i64, Vec<u8>)],
    page_size: u32,
    usable: u32,
) -> Result<u32> {
    // Encode leaf cells first (overflow pages are allocated immediately).
    let mut cells = Vec::with_capacity(entries.len());
    for (rowid, payload) in entries {
        cells.push(table_leaf_cell(*rowid, payload, alloc, usable));
    }
    let capacity_full = page_size as usize;
    let capacity_first = page_size as usize - PAGE1_HEADER_SKIP;
    let sizes: Vec<usize> = cells.iter().map(Vec::len).collect();

    // Fast path: everything fits on page 1 as a leaf.
    let single_fit = 8 + cells.len() * 2 + sizes.iter().sum::<usize>() <= capacity_first;
    if single_fit {
        let rendered = render_page(PAGE_LEAF_TABLE, &cells, None, page_size, PAGE1_HEADER_SKIP);
        alloc.put(1, rendered);
        return Ok(1);
    }

    // Otherwise pack leaves at full capacity and put an interior root on
    // page 1. The `sqlite_master` of SQLKit files is tiny (one row per
    // table/index), so one interior level always suffices; a defensive
    // second level is built if it ever outgrows page 1.
    let mut level: Vec<(u32, i64)> = Vec::new();
    let mut offset = 0usize;
    for count in pack_cells(&sizes, capacity_full, 8) {
        let group = &cells[offset..offset + count];
        let max_rowid = entries[offset + count - 1].0;
        let pgno = alloc.alloc();
        alloc.put(pgno, render_page(PAGE_LEAF_TABLE, group, None, page_size, 0));
        level.push((pgno, max_rowid));
        offset += count;
    }
    debug_assert!(!level.is_empty());
    // Build interior pages until the root fits on page 1.
    let mut root_cells = Vec::new();
    let mut rightmost = 0u32;
    // First try a single interior root.
    {
        let mut trial = Vec::new();
        for slot in 0..level.len() - 1 {
            let mut cell = Vec::new();
            cell.extend_from_slice(&level[slot].0.to_be_bytes());
            cell.extend_from_slice(&encode_varint(level[slot].1 as u64));
            trial.push(cell);
        }
        let need: usize = 12 + trial.len() * 2 + trial.iter().map(Vec::len).sum::<usize>();
        if need <= capacity_first {
            root_cells = trial;
            rightmost = level[level.len() - 1].0;
        }
    }
    if rightmost == 0 {
        // Extremely large schema: add another interior level with fresh pages
        // and root those instead (still tiny in practice).
        let mut upper: Vec<(u32, i64)> = Vec::new();
        let mut index = 0usize;
        while index < level.len() {
            let mut parent_cells = Vec::new();
            let mut end = index + 1;
            let mut used = 12usize;
            while end < level.len() {
                let probe = 4 + varint_len(level[end - 1].1 as u64);
                if used + 2 + probe > capacity_full {
                    break;
                }
                used += 2 + probe;
                end += 1;
            }
            for slot in index..end - 1 {
                let mut cell = Vec::new();
                cell.extend_from_slice(&level[slot].0.to_be_bytes());
                cell.extend_from_slice(&encode_varint(level[slot].1 as u64));
                parent_cells.push(cell);
            }
            let right = level[end - 1].0;
            let max_key = level[end - 1].1;
            let pgno = alloc.alloc();
            alloc.put(
                pgno,
                render_page(PAGE_INTERIOR_TABLE, &parent_cells, Some(right), page_size, 0),
            );
            upper.push((pgno, max_key));
            index = end;
        }
        for slot in 0..upper.len() - 1 {
            let mut cell = Vec::new();
            cell.extend_from_slice(&upper[slot].0.to_be_bytes());
            cell.extend_from_slice(&encode_varint(upper[slot].1 as u64));
            root_cells.push(cell);
        }
        rightmost = upper[upper.len() - 1].0;
    }
    let rendered = render_page(PAGE_INTERIOR_TABLE, &root_cells, Some(rightmost), page_size, PAGE1_HEADER_SKIP);
    alloc.put(1, rendered);
    Ok(1)
}

/// Copy the stashed page-1 content over the image. Page 1 was rendered with
/// the header area zeroed; the database header was written first, so only
/// the B-Tree bytes after offset 100 are copied.
fn insert_master_root(image: &mut [u8], alloc: &Allocator, page_size: u32) -> Result<()> {
    let root = alloc.pages.get(&1).ok_or_else(|| SqlError::Custom("master root missing".into()))?;
    if root.len() != page_size as usize {
        return Err(SqlError::Custom("master root has wrong size".into()));
    }
    image[PAGE1_HEADER_SKIP..page_size as usize].copy_from_slice(&root[PAGE1_HEADER_SKIP..]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths, fsync helpers, WAL honesty, journal recovery.
// ---------------------------------------------------------------------------

/// Journal file next to the database (`<db>-journal`, SQLite convention).
pub fn journal_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push("-journal");
    PathBuf::from(name)
}

/// Write-ahead log file next to the database (`<db>-wal`).
pub fn wal_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn fsync_file(file: &File) -> Result<()> {
    file.sync_all().map_err(SqlError::Io)
}

fn fsync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).map(PathBuf::from);
    if let Some(dir) = parent {
        if let Ok(handle) = File::open(&dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Secure sidecar-file creation.
// ---------------------------------------------------------------------------

/// Counter for process-unique temp-file names.
fn tmp_counter() -> &'static AtomicU64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

/// Refuse symlinked sidecars: an attacker-prepared symlink at a tmp or
/// journal path would otherwise redirect a truncate or become a rename
/// target outside the database directory.
fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SqlError::custom(format!(
            "refusing to use symlinked sidecar file at {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SqlError::Io(e)),
    }
}

/// `OpenOptions` with owner-only permissions applied AT CREATION time, so
/// database bytes are never briefly world-readable before a later chmod.
fn secure_options() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

/// Create a uniquely named temp file next to `path` with `O_EXCL` semantics
/// (`create_new`): a pre-existing file or symlink is never truncated or
/// followed. Retries with a fresh counter on (unlikely) name collision.
fn create_unique_tmp(path: &Path) -> Result<(PathBuf, File)> {
    let pid = std::process::id();
    for _ in 0..100 {
        let n = tmp_counter().fetch_add(1, Ordering::Relaxed);
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{pid}.{n}.sqlkit-tmp"));
        let tmp = PathBuf::from(name);
        refuse_symlink(&tmp)?;
        let mut opts = secure_options();
        match opts.create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(SqlError::Io(e)),
        }
    }
    Err(SqlError::custom(format!(
        "could not create unique temp file next to {}",
        path.display()
    )))
}

/// Create (or truncate) the fixed-name rollback journal. The name must stay
/// `<db>-journal` so crash recovery finds it; symlinks are refused and the
/// file is created owner-only.
fn create_journal_file(journal: &Path) -> Result<File> {
    refuse_symlink(journal)?;
    let mut opts = secure_options();
    opts.create(true).truncate(true).open(journal).map_err(SqlError::Io)
}

/// `create_unique_tmp` for the legacy snapshot path in [`crate::pager`].
/// Kept crate-visible so both persist paths share one hardened helper.
#[doc(hidden)]
pub fn create_unique_tmp_for(path: &Path) -> Result<(PathBuf, File)> {
    create_unique_tmp(path)
}

/// `refuse_symlink` for the legacy snapshot path in [`crate::pager`].
#[doc(hidden)]
pub fn refuse_symlink_for(path: &Path) -> Result<()> {
    refuse_symlink(path)
}

/// Refuse to open a database with uncheckpointed WAL frames. SQLKit is a
/// rollback-journal engine and never replays `-wal` content, so reading the
/// main file would silently return stale rows. Callers surface `Unsupported`.
pub fn check_wal(path: &Path) -> Result<()> {
    let wal = wal_path(path);
    match fs::metadata(&wal) {
        Ok(meta) if meta.len() > 0 => Err(SqlError::unsupported(format!(
            "database has an uncheckpointed write-ahead log ({} bytes at {}); \
             checkpoint it with SQLite first",
            meta.len(),
            wal.display()
        ))),
        _ => Ok(()),
    }
}

/// True for legacy snapshot files (`TSQL01` marker after the header).
pub fn is_snapshot_bytes(raw: &[u8]) -> bool {
    raw.len() >= 106 && raw[100..106] == *crate::pager::SQLKIT_MARKER
}

/// Roll back a leftover SQLKit journal, if present. Returns `true` when a
/// recovery happened. A foreign (real SQLite) journal is refused with
/// `Unsupported`; an empty journal file is removed.
pub fn recover_if_needed(path: &Path) -> Result<bool> {
    let journal = journal_path(path);
    let raw = match fs::read(&journal) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(SqlError::Io(e)),
    };
    if raw.is_empty() {
        let _ = fs::remove_file(&journal);
        return Ok(false);
    }
    if raw.len() >= 8 && raw[0..8] == SQLITE_JOURNAL_MAGIC {
        return Err(SqlError::unsupported(format!(
            "database has a foreign rollback journal at {}; open it with SQLite first",
            journal.display()
        )));
    }
    if raw.len() < 12 || raw[0..8] != *JOURNAL_MAGIC {
        return Err(SqlError::unsupported(format!(
            "unrecognized journal file at {}; refusing to open",
            journal.display()
        )));
    }
    let version = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if version != JOURNAL_VERSION {
        return Err(SqlError::unsupported(format!(
            "unsupported SQLKit journal version {version} at {}",
            journal.display()
        )));
    }
    drop(raw);
    // Stream the original content back: journals can be hundreds of
    // megabytes, so the payload is copied in chunks through a temp file
    // (atomic rename) instead of loading it into RAM.
    let (tmp, mut out) = create_unique_tmp(path)?;
    {
        use std::io::Read;
        let mut src = File::open(&journal)?;
        let mut header = [0u8; 12];
        let mut filled = 0usize;
        while filled < header.len() {
            match src.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(SqlError::Io(e)),
            }
        }
        let mut chunk = [0u8; 65536];
        loop {
            match src.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => out.write_all(&chunk[..n]).map_err(SqlError::Io)?,
                Err(e) => return Err(SqlError::Io(e)),
            }
        }
        out.flush().map_err(SqlError::Io)?;
        fsync_file(&out)?;
    }
    drop(out);
    fsync_parent(&tmp)?;
    refuse_symlink(path)?;
    fs::rename(&tmp, path)?;
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
    fsync_parent(path)?;
    fs::remove_file(&journal)?;
    fsync_parent(path)?;
    Ok(true)
}

/// Previous-file metadata for [`persist_native`]: total length plus the
/// first bytes (cookie, counter and snapshot detection need no more).
/// The full previous content is streamed into the journal on demand, so
/// multi-hundred-megabyte files never load into RAM here.
fn read_old_meta(path: &Path) -> Result<Option<(u64, Vec<u8>)>> {
    let total = match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SqlError::Io(e)),
        Ok(meta) => meta.len(),
    };
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut prefix = [0u8; 128];
    let mut filled = 0usize;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(SqlError::Io(e)),
        }
    }
    Ok(Some((total, prefix[..filled].to_vec())))
}

fn old_page_count(total_len: u64, prefix: &[u8]) -> u32 {
    if is_snapshot_bytes(prefix) || (prefix.len() < 100 && total_len < 100) {
        return 0;
    }
    // Only trust the count when the file is really paged at 4096 bytes.
    if total_len % u64::from(NATIVE_PAGE_SIZE) != 0 {
        return 0;
    }
    match crate::btree::parse_file_header(&prefix[0..100.min(prefix.len())]) {
        Ok(header) if header.page_size == NATIVE_PAGE_SIZE => {
            u32::try_from(total_len / u64::from(NATIVE_PAGE_SIZE)).unwrap_or(u32::MAX)
        }
        _ => 0,
    }
}

fn old_cookie_and_counter(prefix: &[u8]) -> (u32, u32) {
    if prefix.len() < 100 || is_snapshot_bytes(prefix) {
        return (1, 0);
    }
    let cookie = u32::from_be_bytes([prefix[40], prefix[41], prefix[42], prefix[43]]);
    let counter = u32::from_be_bytes([prefix[24], prefix[25], prefix[26], prefix[27]]);
    (cookie.max(1), counter)
}

// ---------------------------------------------------------------------------
// Top-level persist entry point used by `Connection`.
// ---------------------------------------------------------------------------

/// One index definition handed to [`persist_native`]: display name, table
/// display name, indexed columns and the `CREATE INDEX` SQL text.
#[derive(Clone, Debug)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub sql: String,
}

/// Durable state the connection keeps between commits.
#[derive(Clone, Debug)]
pub struct PersistState {
    pub schema_cookie: u32,
    pub schema_dirty: bool,
    pub change_counter: u32,
}

impl PersistState {
    pub fn fresh() -> Self {
        Self { schema_cookie: 1, schema_dirty: false, change_counter: 1 }
    }
}

/// Persist outcome returned to the connection.
#[derive(Clone, Debug)]
pub struct PersistOutcome {
    pub schema_cookie: u32,
    pub change_counter: u32,
    pub page_count: u32,
}

/// Rebuild the database file from in-memory tables with rollback-journal
/// crash safety (journal, fsync journal, write image, fsync image, rename,
/// fsync directory, delete journal). Snapshot files migrate to the native
/// layout through the same atomic rename.
#[allow(clippy::too_many_arguments)]
pub fn persist_native(
    path: &Path,
    tables: &HashMap<String, Table>,
    schemas: &HashMap<String, String>,
    display_names: &HashMap<String, String>,
    indexes: &HashMap<String, IndexDef>,
    rowids: &HashMap<String, Vec<i64>>,
    state: &PersistState,
) -> Result<PersistOutcome> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let old = read_old_meta(path)?;
    let (base_cookie, base_counter) = old
        .as_ref()
        .map(|(_, prefix)| old_cookie_and_counter(prefix))
        .unwrap_or((1, 0));
    let cookie = if state.schema_dirty {
        base_cookie.wrapping_add(1).max(1)
    } else if old.is_none() {
        state.schema_cookie.max(1)
    } else {
        base_cookie
    };
    let counter = base_counter.wrapping_add(1).max(1);
    let old_pages = old
        .as_ref()
        .map(|(total, prefix)| old_page_count(*total, prefix))
        .unwrap_or(0);

    // Assemble builder inputs in a deterministic order.
    let mut table_inputs = Vec::with_capacity(tables.len());
    for (key, table) in tables {
        let sql = schemas
            .get(key)
            .cloned()
            .unwrap_or_else(|| create_table_sql(&table.name, &table.columns));
        let display = display_names.get(key).cloned().unwrap_or_else(|| table.name.clone());
        let alias = rowid_alias_of(&table.columns);
        let ids = rowids.get(key).cloned().unwrap_or_else(|| synthesize_rowids(table, alias));
        let rows: Vec<(i64, Vec<Value>)> = table
            .rows
            .iter()
            .cloned()
            .zip(ids.into_iter().chain(std::iter::repeat(0)))
            .take(table.rows.len())
            .map(|(values, id)| (id, values))
            .collect();
        table_inputs.push(TableInput { name: display, sql, rows, rowid_alias: alias });
    }
    // Index entries are derived from the current table rows, so file-side
    // indexes always match the data (foreign index definitions included).
    let mut index_inputs = Vec::new();
    for entry in indexes.values() {
        let table_key = entry.table.to_ascii_lowercase();
        let (Some(table), Some(ids)) = (tables.get(&table_key), rowids.get(&table_key)) else {
            continue;
        };
        let positions: Vec<usize> = entry
            .columns
            .iter()
            .map(|c| table.columns.iter().position(|col| col.name.eq_ignore_ascii_case(c)))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        if positions.len() != entry.columns.len() {
            continue;
        }
        let mut entries = Vec::with_capacity(table.rows.len());
        for (row, rowid) in table.rows.iter().zip(ids.iter().chain(std::iter::repeat(&0)).take(table.rows.len())) {
            let mut key: Vec<Value> = positions.iter().map(|i| row[*i].clone()).collect();
            key.push(Value::Integer(*rowid));
            entries.push(key);
        }
        index_inputs.push(IndexInput {
            name: entry.name.clone(),
            sql: entry.sql.clone(),
            table_key,
            entries,
        });
    }

    let image = build_database_image(BuildOptions {
        tables: table_inputs,
        indexes: index_inputs,
        schema_cookie: cookie,
        change_counter: counter,
        old_page_count: old_pages,
    })?;

    // Rollback journal first: stream the previous content in chunks (never
    // fully into RAM), then fsync. Owner-only file, symlinks refused.
    if old.is_some() {
        let journal = journal_path(path);
        {
            let mut file = create_journal_file(&journal)?;
            file.write_all(JOURNAL_MAGIC)?;
            file.write_all(&JOURNAL_VERSION.to_be_bytes())?;
            {
                use std::io::Read;
                let mut src = File::open(path)?;
                let mut chunk = [0u8; 65536];
                loop {
                    match src.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => file.write_all(&chunk[..n])?,
                        Err(e) => return Err(SqlError::Io(e)),
                    }
                }
            }
            file.flush()?;
            fsync_file(&file)?;
        }
        fsync_parent(&journal)?;
        crash_here(path, "after_journal")?;
    }

    // Atomic image replacement in the same directory (unique temp name,
    // O_EXCL creation, owner-only permissions).
    let (tmp, mut file) = create_unique_tmp(path)?;
    {
        file.write_all(&image)?;
        file.flush()?;
        fsync_file(&file)?;
    }
    drop(file);
    fsync_parent(&tmp)?;
    refuse_symlink(path)?;
    fs::rename(&tmp, path)?;
    // Re-sync the file and the directory so the rename is durable, then
    // delete the journal (commit record) and sync the directory again.
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
    fsync_parent(path)?;
    let journal = journal_path(path);
    if journal.exists() {
        fs::remove_file(&journal)?;
        fsync_parent(path)?;
    }
    Ok(PersistOutcome {
        schema_cookie: cookie,
        change_counter: counter,
        page_count: u32::try_from(image.len() as u64 / u64::from(NATIVE_PAGE_SIZE))
            .unwrap_or(u32::MAX),
    })
}

/// Create a fresh minimal native database (empty `sqlite_master` on page 1).
pub fn create_empty_db(path: &Path) -> Result<PersistOutcome> {
    persist_native(
        path,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &PersistState::fresh(),
    )
}

/// Position of the single `INTEGER PRIMARY KEY` rowid alias, if any.
pub fn rowid_alias_of(columns: &[Column]) -> Option<usize> {
    let mut found = None;
    for (index, column) in columns.iter().enumerate() {
        if column.primary_key && column.coltype.eq_ignore_ascii_case("INTEGER") {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

/// Assign rowids for rows without stored ones: explicit `INTEGER PRIMARY
/// KEY` values win, otherwise `max + 1` (SQLite `max+1 per table` rule).
pub fn synthesize_rowids(table: &Table, alias: Option<usize>) -> Vec<i64> {
    let mut ids = Vec::with_capacity(table.rows.len());
    let mut max: i64 = 0;
    if let Some(position) = alias {
        for row in &table.rows {
            match row.get(position) {
                Some(Value::Integer(v)) => {
                    max = max.max(*v);
                    ids.push(*v);
                }
                _ => {
                    max = max.saturating_add(1);
                    ids.push(max);
                }
            }
        }
        // Fill gaps left by NULL aliases so later inserts continue at max+1.
        let mut running = 0i64;
        for id in ids.iter_mut() {
            running = running.max(*id);
        }
        let _ = running;
    } else {
        for _ in &table.rows {
            max = max.saturating_add(1);
            ids.push(max);
        }
    }
    ids
}

/// Next rowid for a table (`max + 1`, or 1 when empty).
pub fn next_rowid(ids: &[i64]) -> i64 {
    ids.iter().copied().max().unwrap_or(0).wrapping_add(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::{decode_varint, parse_record, table_leaf_local_len, TextEncoding};

    #[test]
    fn varint_roundtrip_vectors() {
        for value in [0u64, 1, 127, 128, 300, 16_383, 16_384, 0x3_BF1F, 0xFFFF_FFFF, u64::MAX - 1, u64::MAX] {
            let encoded = encode_varint(value);
            assert_eq!(encoded.len(), varint_len(value));
            let (decoded, used) = decode_varint(&encoded).unwrap();
            assert_eq!((decoded, used), (value, encoded.len()), "varint {value}");
        }
        assert_eq!(encode_varint(0x3_BF1F), vec![0x8E, 0xFE, 0x1F]);
        assert_eq!(encode_varint(0xFFFF_FFFF), vec![0x8F, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert_eq!(encode_varint(u64::MAX).len(), 9);
    }

    #[test]
    fn integer_serial_minimal_widths() {
        assert_eq!(integer_serial(0), 8);
        assert_eq!(integer_serial(1), 9);
        assert_eq!(integer_serial(-1), 1);
        assert_eq!(integer_serial(127), 1);
        assert_eq!(integer_serial(128), 2);
        assert_eq!(integer_serial(-129), 2);
        assert_eq!(integer_serial(8_388_607), 3);
        assert_eq!(integer_serial(8_388_608), 4);
        assert_eq!(integer_serial(2_147_483_647), 4);
        assert_eq!(integer_serial(2_147_483_648), 5);
        assert_eq!(integer_serial(i64::MAX), 6);
        assert_eq!(integer_serial(i64::MIN), 6);
    }

    #[test]
    fn record_roundtrip_all_types() {
        let values = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-70000),
            Value::Integer(i64::MAX),
            Value::Real(2.5),
            Value::Text(String::new()),
            Value::Text("hello 'quoted' \u{00e4}\u{2713}".to_owned()),
            Value::Blob(vec![]),
            Value::Blob(vec![0x00, 0xFF, 0x13, 0x37]),
        ];
        let payload = encode_record(&values);
        let back = parse_record(&payload, TextEncoding::Utf8).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn overflow_formula_matches_reader() {
        // The writer must spill exactly where the reader expects it.
        for len in [0u64, 100, 4061, 4062, 5000, 100_000] {
            let local = table_leaf_local_len(len, NATIVE_PAGE_SIZE);
            assert!(local <= (NATIVE_PAGE_SIZE - 35) as usize);
            if len <= (NATIVE_PAGE_SIZE - 35) as u64 {
                assert_eq!(local as u64, len);
            }
        }
        // Index pages spill earlier than table leaves.
        assert!(index_leaf_local_len(5000, NATIVE_PAGE_SIZE) <= (NATIVE_PAGE_SIZE - 35) as usize);
        assert_eq!(index_leaf_local_len(100, NATIVE_PAGE_SIZE), 100);
    }

    #[test]
    fn freelist_chain_links_surplus_pages() {
        let usable = NATIVE_PAGE_SIZE;
        let pages: Vec<u32> = (10..15).collect();
        let chain = FreelistChain::from_pages(&pages, usable);
        assert_eq!(chain.total(), 5);
        assert_eq!(chain.first_trunk(), 10);
        // 5 pages fit one trunk (capacity is ~1022 leaves).
        assert_eq!(chain.trunk_count(), 1);
        let mut image = vec![0u8; 20 * page_size_len()];
        chain.render_into(&mut image, NATIVE_PAGE_SIZE);
        let start = 9 * NATIVE_PAGE_SIZE as usize;
        assert_eq!(&image[start..start + 4], &0u32.to_be_bytes());
        assert_eq!(&image[start + 4..start + 8], &4u32.to_be_bytes());
        assert_eq!(&image[start + 8..start + 12], &11u32.to_be_bytes());
    }

    fn page_size_len() -> usize {
        NATIVE_PAGE_SIZE as usize
    }

    #[test]
    fn empty_image_loads_back() {
        let image = build_database_image(BuildOptions {
            tables: vec![],
            indexes: vec![],
            schema_cookie: 1,
            change_counter: 1,
            old_page_count: 0,
        })
        .unwrap();
        assert_eq!(image.len(), page_size_len());
        assert!(image.starts_with(crate::pager::SQLITE_MAGIC));
        assert_eq!(u32::from_be_bytes([image[28], image[29], image[30], image[31]]), 1);
    }

    #[test]
    fn table_image_matches_reader_math() {
        let rows: Vec<(i64, Vec<Value>)> = (0..50)
            .map(|i| {
                (
                    i + 1,
                    vec![
                        Value::Integer(i + 1),
                        Value::Text(format!("name-{i}")),
                        Value::Real(i as f64 * 1.5),
                    ],
                )
            })
            .collect();
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)".to_owned();
        let image = build_database_image(BuildOptions {
            tables: vec![TableInput {
                name: "t".to_owned(),
                sql,
                rows,
                rowid_alias: Some(0),
            }],
            indexes: vec![],
            schema_cookie: 3,
            change_counter: 2,
            old_page_count: 0,
        })
        .unwrap();
        // Page 1 must be a table B-Tree and the schema cookie must survive.
        assert!(image[100] == PAGE_LEAF_TABLE || image[100] == PAGE_INTERIOR_TABLE);
        assert_eq!(u32::from_be_bytes([image[40], image[41], image[42], image[43]]), 3);
    }
}
