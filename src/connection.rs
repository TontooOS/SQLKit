//! `Connection`: ruslite-compatible database handle over the basis engine.

use crate::error::{Result, SqlError};
use crate::pager;
use crate::parser::{self, ColumnDef, CompiledWhere, Expr, Stmt};
use crate::statement::Statement;
use crate::transaction::Transaction;
use crate::value::{FromValue, IntoParams, Value};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Column schema stored per table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub coltype: String,
    pub primary_key: bool,
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<Value>,
}

impl From<&ColumnDef> for Column {
    fn from(def: &ColumnDef) -> Self {
        Self {
            name: def.name.clone(),
            coltype: def.coltype.clone(),
            primary_key: def.primary_key,
            not_null: def.not_null,
            default: def.default.clone(),
        }
    }
}

/// In-memory table: schema plus row store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }
}

pub(crate) struct Inner {
    pub path: Option<PathBuf>,
    pub tables: HashMap<String, Table>,
    /// Index definitions by lowercase name (display name, table, columns,
    /// `CREATE INDEX` SQL). Persisted into `sqlite_master` by the native
    /// writer with contents rebuilt from the table rows.
    pub indexes: HashMap<String, IndexEntry>,
    pub pragmas: HashMap<String, String>,
    pub changes: usize,
    pub last_rowid: i64,
    pub in_transaction: bool,
    pub backup: Option<Backup>,
    /// In-memory primary-key lookup cache: table key to (key value, row
    /// position). Never serialized; rebuilt lazily from `tables` and
    /// invalidated on any `UPDATE`, `DELETE` or rollback. Turns the
    /// per-insert uniqueness scan from O(rows) into O(1).
    pub pk_cache: HashMap<String, HashMap<PkKey, usize>>,
    /// Verbatim `CREATE TABLE` texts by lowercase table name. Foreign files
    /// keep their original text; new tables use the canonical rendering.
    pub schemas: HashMap<String, String>,
    /// Display names by lowercase table name (original casing).
    pub display_names: HashMap<String, String>,
    /// Rowids parallel to each table's rows (same order, same length).
    /// Assigned as `max + 1` per table; `INTEGER PRIMARY KEY` values double
    /// as rowids, exactly like SQLite.
    pub rowids: HashMap<String, Vec<i64>>,
    /// Cached per-table rowid maximum (`max + 1` is the next free rowid).
    /// Maintained on every write path so bulk inserts never rescan the
    /// rowid vector (which would be O(rows) per row). Never serialized;
    /// rebuilt from `rowids` on load and restored from `backup` on rollback.
    pub rowid_max: HashMap<String, i64>,
    /// Native persist state: schema cookie, dirty flag, change counter.
    pub persist: crate::btree_write::PersistState,
    /// Backing file layout: transient memory, legacy snapshot (migrated on
    /// the first write), or native B-Tree.
    pub file_kind: FileKind,
    /// Whether table data is loaded. File-backed connections open lazily:
    /// `open` only validates the header and journals, the full load happens
    /// on the first data access (`ensure_loaded`). In-memory connections
    /// start loaded.
    pub loaded: bool,
    /// True once an unpersisted write changed durable state. Lets
    /// `persist_if_needed` skip file rewrites when nothing changed (for
    /// example a freshly opened connection that is only read or closed).
    pub dirty: bool,
}

/// One in-memory index definition.
#[derive(Clone, Debug)]
pub(crate) struct IndexEntry {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub sql: String,
}

/// Transaction backup: tables plus the parallel native-write state so a
/// rollback restores rowids, schemas and indexes together.
#[derive(Clone, Debug, Default)]
pub(crate) struct Backup {
    pub tables: HashMap<String, Table>,
    pub schemas: HashMap<String, String>,
    pub display_names: HashMap<String, String>,
    pub rowids: HashMap<String, Vec<i64>>,
    pub rowid_max: HashMap<String, i64>,
    pub indexes: HashMap<String, IndexEntry>,
}

/// Backing file layout of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    Memory,
    Snapshot,
    Native,
}

/// Hashable primary-key value for [`Inner::pk_cache`]. Real keys use raw
/// bits so every value maps deterministically.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum PkKey {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl PkKey {
    fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(v) => Self::Integer(*v),
            Value::Real(v) => Self::Real(v.to_bits()),
            Value::Text(s) => Self::Text(s.clone()),
            Value::Blob(b) => Self::Blob(b.clone()),
        }
    }
}

/// Database connection. Mirrors the `rusqlite::Connection` subset used by
/// CoreData: `open`, `execute`, `execute_batch`, `prepare`, `transaction`.
pub struct Connection {
    pub(crate) inner: RefCell<Inner>,
    /// Parsed-statement cache keyed by the exact SQL text. Parsing depends
    /// only on the SQL string, never on schema or data state, so entries
    /// never go stale. Kept in its own `RefCell` (not inside `Inner`) so
    /// `execute` can run directly from the cache borrow while `run_stmt`
    /// mutably borrows `inner`. Bounded by [`STMT_CACHE_CAP`].
    stmt_cache: RefCell<HashMap<String, Stmt>>,
}

/// Maximum statements held by [`Connection::stmt_cache`]. When the cache is
/// full it is cleared before inserting; correctness is unaffected because the
/// cache only avoids re-parsing (a miss simply parses again).
const STMT_CACHE_CAP: usize = 256;

impl Connection {
    fn fresh(path: Option<PathBuf>) -> Self {
        let loaded = path.is_none();
        Self {
            inner: RefCell::new(Inner {
                path,
                tables: HashMap::new(),
                indexes: HashMap::new(),
                pragmas: HashMap::new(),
                changes: 0,
                last_rowid: 0,
                in_transaction: false,
                backup: None,
                pk_cache: HashMap::new(),
                schemas: HashMap::new(),
                display_names: HashMap::new(),
                rowids: HashMap::new(),
                rowid_max: HashMap::new(),
                persist: crate::btree_write::PersistState::fresh(),
                file_kind: FileKind::Memory,
                loaded,
                dirty: false,
            }),
            stmt_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Open (or create) a file-backed database.
    ///
    /// The open refuses files with uncheckpointed `-wal` content
    /// (`Unsupported`, never a silent stale read) and rolls back a leftover
    /// SQLKit `-journal` before reading. New files are created as real
    /// minimal native SQLite databases. The open itself is LAZY: it only
    /// validates the header and journals, so opening is O(1) regardless of
    /// file size; table data loads on the first data access (see
    /// `ensure_loaded`). Legacy snapshot files load as before and migrate
    /// to the native layout on the first write.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        crate::btree_write::check_wal(&path)?;
        let _recovered = crate::btree_write::recover_if_needed(&path)?;
        if !pager::file_exists(&path) {
            let conn = Self::fresh(Some(path.clone()));
            pager::create_new(&path)?;
            conn.inner.borrow_mut().file_kind = FileKind::Native;
            return Ok(conn);
        }
        // Header validation first: non-SQLite files stay `NotSqliteFile`.
        let file_kind = pager::peek_kind(&path)?;
        let conn = Self::fresh(Some(path));
        conn.inner.borrow_mut().file_kind = file_kind;
        Ok(conn)
    }

    /// Load table data from disk when it is first needed. File-backed
    /// connections start unloaded so `open` stays O(1); the first `execute`,
    /// `prepare` or transaction pulls the data in. In-memory connections
    /// and already-loaded ones return immediately.
    pub(crate) fn ensure_loaded(&self) -> Result<()> {
        if self.inner.borrow().loaded {
            return Ok(());
        }
        self.load_from_disk()
    }

    /// Try `SELECT COUNT(*)` without materializing the tables. Returns
    /// `Ok(Some(n))` only for the unloaded file-backed case with a plain
    /// `COUNT(*)` over one real B-Tree table (cell walk, no record decode);
    /// `Ok(None)` means "use the normal path" (already loaded, snapshot
    /// file, filtered or complex query). Unknown tables error here so the
    /// caller never loads a whole file just to report "no such table".
    pub(crate) fn fast_count_star(&self, stmt: &Stmt, bound: &[Value]) -> Result<Option<i64>> {
        let (table, filter) = match stmt {
            Stmt::Select { table, filter, count_star: true, joins, group_by, having, distinct, .. }
                if joins.is_empty() && group_by.is_empty() && having.is_none() && !distinct =>
            {
                (table.clone(), filter.clone())
            }
            _ => return Ok(None),
        };
        if filter.is_some() || !bound.is_empty() {
            return Ok(None);
        }
        let (loaded, kind, path) = {
            let inner = self.inner.borrow();
            (inner.loaded, inner.file_kind, inner.path.clone())
        };
        if loaded {
            return Ok(None);
        }
        let Some(path) = path else {
            return Ok(None);
        };
        if kind != FileKind::Native {
            return Ok(None);
        }
        match crate::btree::count_table_rows(&path, &table) {
            Ok(n) => Ok(Some(n)),
            Err(SqlError::SqliteFailure { code: 1, .. }) => Err(SqlError::SqliteFailure {
                code: 1,
                message: format!("no such table: {table}"),
            }),
            Err(_) => Ok(None),
        }
    }

    fn load_from_disk(&self) -> Result<()> {
        let (file_kind, path) = {
            let inner = self.inner.borrow();
            let Some(path) = inner.path.clone() else {
                self.inner.borrow_mut().loaded = true;
                return Ok(());
            };
            (inner.file_kind, path)
        };
        match file_kind {
            FileKind::Memory => {}
            FileKind::Snapshot => {
                let tables = pager::load(&path)?;
                let mut inner = self.inner.borrow_mut();
                for (key, table) in tables {
                    let alias = crate::btree_write::rowid_alias_of(&table.columns);
                    let ids = crate::btree_write::synthesize_rowids(&table, alias);
                    inner.rowid_max.insert(key.clone(), ids.iter().copied().max().unwrap_or(0));
                    inner.schemas.insert(
                        key.clone(),
                        crate::btree_write::create_table_sql(&table.name, &table.columns),
                    );
                    inner.display_names.insert(key.clone(), table.name.clone());
                    inner.rowids.insert(key.clone(), ids);
                    inner.tables.insert(key, table);
                }
            }
            FileKind::Native => {
                let detailed = crate::btree::load_foreign_detailed(&path)?;
                let mut inner = self.inner.borrow_mut();
                inner.persist = crate::btree_write::PersistState::fresh();
                inner.persist.schema_cookie = detailed.schema_cookie.max(1);
                inner.tables = detailed.tables;
                inner.rowids = detailed.rowids;
                inner.schemas = detailed.schemas;
                inner.display_names = detailed.display_names;
                for index in detailed.indexes {
                    inner.indexes.insert(
                        index.name.to_ascii_lowercase(),
                        IndexEntry {
                            name: index.name.clone(),
                            table: index.table.clone(),
                            columns: index.columns.clone(),
                            sql: index.sql.clone(),
                        },
                    );
                }
                // Defensive repair: rowid vectors always parallel the rows.
                let keys: Vec<String> = inner.tables.keys().cloned().collect();
                for key in keys {
                    let table = inner.tables.get(&key).ok_or_else(|| missing_table(&key))?;
                    let ok = inner.rowids.get(&key).map(|ids| ids.len() == table.rows.len()).unwrap_or(false);
                    if !ok {
                        let alias = crate::btree_write::rowid_alias_of(&table.columns);
                        let ids = crate::btree_write::synthesize_rowids(table, alias);
                        inner.rowids.insert(key, ids);
                    }
                }
                let maxima: Vec<(String, i64)> = inner
                    .rowids
                    .iter()
                    .map(|(key, ids)| (key.clone(), ids.iter().copied().max().unwrap_or(0)))
                    .collect();
                for (key, max) in maxima {
                    inner.rowid_max.insert(key, max);
                }
            }
        }
        self.inner.borrow_mut().loaded = true;
        Ok(())
    }

    /// Open a transient in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::fresh(None))
    }

    /// Write the native database file when it is due. Skipped while a
    /// transaction is open so bulk loads inside `transaction()` produce
    /// exactly one image at commit instead of one per row; the file then
    /// always equals the last committed state. Snapshot files migrate to
    /// the native B-Tree layout through this same atomic rename.
    pub(crate) fn persist_if_needed(&self) -> Result<()> {
        if self.inner.borrow().in_transaction {
            return Ok(());
        }
        // Nothing changed (freshly opened or only read): skip the file
        // rewrite entirely. Set by `run_stmt` for every write statement.
        if !self.inner.borrow().dirty {
            return Ok(());
        }
        let outcome = {
            let inner = self.inner.borrow();
            let Some(path) = inner.path.clone() else {
                return Ok(());
            };
            let indexes: HashMap<String, crate::btree_write::IndexDef> = inner
                .indexes
                .iter()
                .map(|(key, entry)| {
                    (
                        key.clone(),
                        crate::btree_write::IndexDef {
                            name: entry.name.clone(),
                            table: entry.table.clone(),
                            columns: entry.columns.clone(),
                            sql: entry.sql.clone(),
                        },
                    )
                })
                .collect();
            crate::btree_write::persist_native(
                &path,
                &inner.tables,
                &inner.schemas,
                &inner.display_names,
                &indexes,
                &inner.rowids,
                &inner.persist,
            )?
        };
        let mut inner = self.inner.borrow_mut();
        inner.persist.schema_cookie = outcome.schema_cookie;
        inner.persist.change_counter = outcome.change_counter;
        inner.persist.schema_dirty = false;
        inner.file_kind = FileKind::Native;
        inner.dirty = false;
        Ok(())
    }

    /// Execute a single statement with bound parameters.
    ///
    /// The SQL text is parsed once per distinct string and then served from
    /// the per-connection statement cache; repeated calls (for example bulk
    /// inserts through one reused SQL string) skip tokenizing and parsing
    /// entirely and run straight from the cached statement.
    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        let bound = params.into_params()?;
        self.ensure_loaded()?;
        // Fast path: run directly from the cache borrow (no parse, no clone).
        // `run_stmt` borrows `inner`, a different `RefCell`, so holding the
        // cache borrow across the call is sound; `run_stmt` never touches the
        // cache and never runs user code.
        let cached = self.stmt_cache.borrow();
        if let Some(stmt) = cached.get(sql) {
            let changes = self.run_stmt(stmt, &bound)?;
            let write = is_write(stmt);
            drop(cached);
            if write {
                self.persist_if_needed()?;
            }
            return Ok(changes);
        }
        drop(cached);
        let stmt = parser::parse_one(sql)?;
        let changes = self.run_stmt(&stmt, &bound)?;
        let write = is_write(&stmt);
        self.insert_cached(sql, stmt);
        if write {
            self.persist_if_needed()?;
        }
        Ok(changes)
    }

    /// Insert a freshly parsed statement into the cache, clearing the cache
    /// first when it reached [`STMT_CACHE_CAP`]. Callers must have parsed the
    /// statement from `sql`, so the key always matches the value.
    fn insert_cached(&self, sql: &str, stmt: Stmt) {
        let mut cache = self.stmt_cache.borrow_mut();
        if cache.len() >= STMT_CACHE_CAP {
            cache.clear();
        }
        cache.insert(sql.to_owned(), stmt);
    }

    /// Parse one statement through the cache, returning an owned copy for
    /// `prepare` / `query_row`. Cache misses parse and populate the cache.
    fn parse_cached(&self, sql: &str) -> Result<Stmt> {
        if let Some(stmt) = self.stmt_cache.borrow().get(sql) {
            return Ok(stmt.clone());
        }
        let stmt = parser::parse_one(sql)?;
        self.insert_cached(sql, stmt.clone());
        Ok(stmt)
    }

    /// Run several statements without parameters (also accepts PRAGMA /
    /// SELECT that return rows, like ruslite).
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.ensure_loaded()?;
        let stmts = parser::parse_batch(sql)?;
        for stmt in &stmts {
            let _ = self.run_stmt(stmt, &[])?;
        }
        self.persist_if_needed()?;
        Ok(())
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let stmt = self.parse_cached(sql)?;
        Ok(Statement::new(self, stmt))
    }

    /// Convenience helper mirroring `rusqlite::Connection::query_row`.
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: IntoParams,
        F: FnOnce(&crate::statement::Row) -> Result<T>,
    {
        let mut stmt = self.prepare(sql)?;
        stmt.query_row(params, f)
    }

    /// Begin a transaction; rolls back on drop unless committed.
    pub fn transaction(&self) -> Result<Transaction<'_>> {
        // Load before snapshotting so rollback restores real data.
        self.ensure_loaded()?;
        self.begin_inner()?;
        Ok(Transaction::new(self))
    }

    /// Number of rows changed by the last write.
    pub fn changes(&self) -> usize {
        self.inner.borrow().changes
    }

    /// Checkpoint the WAL (basis: validates the pragma, no-op otherwise).
    pub fn checkpoint(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.pragmas.insert("wal_checkpoint".into(), "TRUNCATE".into());
        inner.dirty = true;
        drop(inner);
        self.persist_if_needed()?;
        Ok(())
    }

    /// Flush and release the handle.
    pub fn close(self) -> Result<()> {
        self.persist_if_needed()?;
        Ok(())
    }

    pub(crate) fn run_stmt(&self, stmt: &Stmt, bound: &[Value]) -> Result<usize> {
        if is_write(stmt) {
            self.inner.borrow_mut().dirty = true;
        }
        match stmt {
            Stmt::Pragma { name, value } => {
                let mut inner = self.inner.borrow_mut();
                let key = name.to_ascii_lowercase();
                if key == "wal_checkpoint" {
                    inner.pragmas.insert(key, value.clone().unwrap_or_default());
                    inner.changes = 0;
                    return Ok(0);
                }
                inner.pragmas.insert(key, value.clone().unwrap_or_default());
                inner.changes = 0;
                Ok(0)
            }
            Stmt::CreateTable {
                name,
                columns,
                if_not_exists,
            } => {
                let mut inner = self.inner.borrow_mut();
                if inner.tables.contains_key(&name.to_ascii_lowercase()) {
                    if *if_not_exists {
                        inner.changes = 0;
                        return Ok(0);
                    }
                    return Err(SqlError::SqliteFailure {
                        code: 1,
                        message: format!("table {name} already exists"),
                    });
                }
                let table = Table {
                    name: name.clone(),
                    columns: columns.iter().map(Column::from).collect(),
                    rows: Vec::new(),
                };
                let key = name.to_ascii_lowercase();
                let sql = crate::btree_write::create_table_sql(name, &table.columns);
                inner.display_names.insert(key.clone(), name.clone());
                inner.schemas.insert(key.clone(), sql);
                inner.rowids.insert(key.clone(), Vec::new());
                inner.rowid_max.insert(key.clone(), 0);
                inner.tables.insert(key, table);
                inner.persist.schema_dirty = true;
                inner.changes = 0;
                Ok(0)
            }
            Stmt::CreateIndex {
                name,
                table,
                columns,
                if_not_exists,
            } => {
                let mut inner = self.inner.borrow_mut();
                if !inner.tables.contains_key(&table.to_ascii_lowercase()) {
                    return Err(SqlError::SqliteFailure {
                        code: 1,
                        message: format!("no such table: {table}"),
                    });
                }
                if inner.indexes.contains_key(&name.to_ascii_lowercase()) && *if_not_exists {
                    inner.changes = 0;
                    return Ok(0);
                }
                let sql = crate::btree_write::create_index_sql(name, table, columns);
                inner.indexes.insert(
                    name.to_ascii_lowercase(),
                    IndexEntry {
                        name: name.clone(),
                        table: table.clone(),
                        columns: columns.clone(),
                        sql,
                    },
                );
                inner.persist.schema_dirty = true;
                inner.changes = 0;
                Ok(0)
            }
            Stmt::Insert {
                or_replace,
                or_ignore,
                table,
                columns,
                rows,
            } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                // Resolve target slots and the primary-key position with a
                // short shared borrow; the mutation below re-borrows.
                let (target, pk) = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| SqlError::SqliteFailure {
                        code: 1,
                        message: format!("no such table: {table}"),
                    })?;
                    let target: Vec<usize> = if columns.is_empty() {
                        (0..tbl.columns.len()).collect()
                    } else {
                        columns
                            .iter()
                            .map(|c| {
                                tbl.column_index(c)
                                    .ok_or_else(|| SqlError::InvalidColumnName(c.clone()))
                            })
                            .collect::<Result<Vec<_>>>()?
                    };
                    (target, tbl.primary_key_index())
                };
                // Resolve all rows before mutating, so a bad parameter leaves
                // the table untouched.
                let mut staged = Vec::with_capacity(rows.len());
                // Defaults for missing columns: existing rows and new inserts
                // use the column default when present, otherwise NULL.
                let defaults: Vec<Value> = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| missing_table(&key))?;
                    tbl.columns.iter().map(|c| c.default.clone().unwrap_or(Value::Null)).collect()
                };
                for exprs in rows {
                    if exprs.len() != target.len() {
                        return Err(SqlError::InvalidParameterCount {
                            expected: target.len(),
                            got: exprs.len(),
                        });
                    }
                    let mut row = defaults.clone();
                    for (slot, expr) in target.iter().zip(exprs.iter()) {
                        row[*slot] = resolve_expr(expr, bound)?;
                    }
                    staged.push(row);
                }
                // Rowid allocation (`max + 1` per table): explicit `INTEGER
                // PRIMARY KEY` values double as rowids, `NULL` aliases are
                // auto-assigned (and filled into the row, SQLite style),
                // every other row takes the next free rowid. The per-table
                // maximum is cached (`rowid_max`) so bulk inserts never
                // rescan the rowid vector (O(rows) per row); it is
                // maintained on every write path and restored on rollback.
                let alias = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| missing_table(&key))?;
                    crate::btree_write::rowid_alias_of(&tbl.columns)
                };
                let base = inner.rowid_max.get(&key).copied().unwrap_or(0);
                let mut next = base.saturating_add(1).max(1);
                // Running maximum over old and staged ids. Derived from the
                // assigned ids (not from `next`, which saturates at
                // i64::MAX) so an explicitly inserted i64::MAX stays cached.
                let mut top = base;
                // Saturation fallback: when every rowid up to i64::MAX is
                // taken (only reachable with explicitly inserted huge ids),
                // reuse the smallest free positive rowid like SQLite does
                // instead of colliding. O(n) once per statement, only in
                // this pathological corner.
                let mut gaps: Option<std::collections::HashSet<i64>> = None;
                if base == i64::MAX {
                    let mut set: std::collections::HashSet<i64> = inner
                        .rowids
                        .get(&key)
                        .map(|ids| ids.iter().copied().collect())
                        .unwrap_or_default();
                    if let Some(pos) = alias {
                        for row in staged.iter() {
                            if let Value::Integer(v) = row[pos] {
                                set.insert(v);
                            }
                        }
                    }
                    gaps = Some(set);
                }
                let mut new_ids = Vec::with_capacity(staged.len());
                for row in staged.iter_mut() {
                    let id = match alias {
                        Some(pos) => match row[pos] {
                            Value::Integer(v) => {
                                if v >= next {
                                    next = v.saturating_add(1);
                                }
                                top = top.max(v);
                                v
                            }
                            _ => {
                                let id = match gaps.as_mut() {
                                    Some(used) => {
                                        let free = smallest_free_rowid(used).ok_or_else(|| {
                                            SqlError::SqliteFailure {
                                                code: 13,
                                                message: "database or disk is full".into(),
                                            }
                                        })?;
                                        used.insert(free);
                                        free
                                    }
                                    None => {
                                        let id = next;
                                        next = next.saturating_add(1).max(1);
                                        id
                                    }
                                };
                                top = top.max(id);
                                row[pos] = Value::Integer(id);
                                id
                            }
                        },
                        None => match gaps.as_mut() {
                            Some(used) => {
                                let free = smallest_free_rowid(used).ok_or_else(|| {
                                    SqlError::SqliteFailure {
                                        code: 13,
                                        message: "database or disk is full".into(),
                                    }
                                })?;
                                used.insert(free);
                                top = top.max(free);
                                free
                            }
                            None => {
                                let id = next;
                                next = next.saturating_add(1).max(1);
                                top = top.max(id);
                                id
                            }
                        },
                    };
                    new_ids.push(id);
                }
                let mut changed = 0;
                if let Some(pk_idx) = pk {
                    // Build the PK cache on first use; afterwards every
                    // uniqueness check is O(1) instead of a full row scan.
                    if !inner.pk_cache.contains_key(&key) {
                        let mut map = HashMap::new();
                        if let Some(tbl) = inner.tables.get(&key) {
                            map.reserve(tbl.rows.len());
                            for (pos, row) in tbl.rows.iter().enumerate() {
                                map.insert(PkKey::of(&row[pk_idx]), pos);
                            }
                        }
                        inner.pk_cache.insert(key.clone(), map);
                    }
                    // NOTE: the cache and the table cannot be borrowed at once
                    // (both go through the same `RefMut`), so each row does
                    // one short cache probe, one table write, then one cache
                    // update in sequence. Each step is O(1).
                    for (row, rid) in staged.into_iter().zip(new_ids) {
                        let lookup = PkKey::of(&row[pk_idx]);
                        let existing = inner
                            .pk_cache
                            .get(&key)
                            .and_then(|m| m.get(&lookup))
                            .copied();
                        if let Some(pos) = existing {
                            if *or_replace {
                                // Keep the stored rowid in sync when the
                                // replacement carries a new alias value.
                                if let Some(slot) = alias {
                                    if let Value::Integer(v) = row[slot] {
                                        if let Some(ids) = inner.rowids.get_mut(&key) {
                                            if let Some(id) = ids.get_mut(pos) {
                                                *id = v;
                                            }
                                        }
                                    }
                                }
                                inner
                                    .tables
                                    .get_mut(&key)
                                    .ok_or_else(|| missing_table(&key))?
                                    .rows[pos] = row;
                                changed += 1;
                            } else if *or_ignore {
                                continue;
                            } else {
                                return Err(SqlError::SqliteFailure {
                                    code: 19,
                                    message: "UNIQUE constraint failed".into(),
                                });
                            }
                        } else {
                            let pos = {
                                let tbl =
                                    inner.tables.get_mut(&key).ok_or_else(|| missing_table(&key))?;
                                let pos = tbl.rows.len();
                                tbl.rows.push(row);
                                pos
                            };
                            inner
                                .rowids
                                .entry(key.clone())
                                .or_default()
                                .push(rid);
                            inner
                                .pk_cache
                                .get_mut(&key)
                                .ok_or_else(|| missing_table(&key))?
                                .insert(lookup, pos);
                            changed += 1;
                        }
                    }
                } else {
                    let count = staged.len();
                    {
                        let tbl = inner.tables.get_mut(&key).ok_or_else(|| missing_table(&key))?;
                        tbl.rows.extend(staged);
                    }
                    inner.rowids.entry(key.clone()).or_default().extend(new_ids);
                    changed = count;
                }
                let top = next.saturating_sub(1).max(base).max(top);
                inner.rowid_max.insert(key.clone(), top);
                inner.last_rowid += changed as i64;
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Select { .. } | Stmt::Union { .. } => Err(SqlError::ExecuteReturnedResults),
            Stmt::AlterTable { table, column } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                let (name, columns) = {
                    let tbl = inner.tables.get_mut(&key).ok_or_else(|| SqlError::SqliteFailure {
                        code: 1,
                        message: format!("no such table: {table}"),
                    })?;
                    if tbl.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&column.name)) {
                        return Err(SqlError::SqliteFailure {
                            code: 1,
                            message: format!("duplicate column name: {}", column.name),
                        });
                    }
                    let fill = column.default.clone().unwrap_or(Value::Null);
                    let new_col = Column::from(column);
                    tbl.columns.push(new_col);
                    for row in tbl.rows.iter_mut() {
                        row.push(fill.clone());
                    }
                    // Regenerate the stored schema text (exotic source
                    // constraints are normalized; see wiki/Pager.md) and mark
                    // the schema cookie dirty.
                    (tbl.name.clone(), tbl.columns.clone())
                };
                inner.schemas.insert(
                    key.clone(),
                    crate::btree_write::create_table_sql(&name, &columns),
                );
                inner.persist.schema_dirty = true;
                inner.changes = 0;
                Ok(0)
            }
            Stmt::Update {
                table,
                assignments,
                filter,
            } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                // Compile assignments and the filter once; per-row work is
                // then pure indexing with no string comparisons.
                let (slots, compiled): (Vec<(usize, Expr)>, Option<CompiledWhere>) = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| SqlError::SqliteFailure {
                        code: 1,
                        message: format!("no such table: {table}"),
                    })?;
                    let slots: Vec<(usize, Expr)> = assignments
                        .iter()
                        .map(|(col, expr)| {
                            tbl.column_index(col)
                                .map(|idx| (idx, expr.clone()))
                                .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let col_names: Vec<String> =
                        tbl.columns.iter().map(|c| c.name.clone()).collect();
                    let map = parser::column_index_map(&col_names);
                    let compiled = filter
                        .as_ref()
                        .map(|f| f.compile(&map, bound))
                        .transpose()?;
                    (slots, compiled)
                };
                let mut changed = 0;
                let alias = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| missing_table(&key))?;
                    crate::btree_write::rowid_alias_of(&tbl.columns)
                };
                {
                    let tbl = inner.tables.get_mut(&key).ok_or_else(|| missing_table(&key))?;
                    for row in tbl.rows.iter_mut() {
                        let keep = match &compiled {
                            Some(f) => f.matches_row(row),
                            None => true,
                        };
                        if keep {
                            for (slot, expr) in &slots {
                                row[*slot] = resolve_expr(expr, bound)?;
                            }
                            changed += 1;
                        }
                    }
                }
                // Keep rowids in sync with the alias column: a new explicit
                // value becomes the rowid, a `NULL` alias takes `max + 1`
                // (and is filled into the row, SQLite style).
                if let Some(pos) = alias {
                    let values: Vec<Value> = inner
                        .tables
                        .get(&key)
                        .ok_or_else(|| missing_table(&key))?
                        .rows
                        .iter()
                        .map(|row| row[pos].clone())
                        .collect();
                    let old_ids = inner.rowids.get(&key).cloned().unwrap_or_default();
                    let mut next = crate::btree_write::next_rowid(&old_ids);
                    let mut new_ids = Vec::with_capacity(values.len());
                    let mut fills: Vec<Option<i64>> = Vec::with_capacity(values.len());
                    for (index, value) in values.iter().enumerate() {
                        match value {
                            Value::Integer(v) => {
                                if *v >= next {
                                    next = v.saturating_add(1);
                                }
                                new_ids.push(*v);
                                fills.push(None);
                            }
                            Value::Null => {
                                new_ids.push(next);
                                fills.push(Some(next));
                                next = next.saturating_add(1).max(1);
                            }
                            _ => {
                                let keep = old_ids.get(index).copied().unwrap_or(next);
                                if old_ids.get(index).is_none() {
                                    next = next.saturating_add(1).max(1);
                                }
                                new_ids.push(keep);
                                fills.push(None);
                            }
                        }
                    }
                    {
                        let tbl = inner.tables.get_mut(&key).ok_or_else(|| missing_table(&key))?;
                        for (row, fill) in tbl.rows.iter_mut().zip(fills.iter()) {
                            if let Some(id) = fill {
                                row[pos] = Value::Integer(*id);
                            }
                        }
                    }
                    inner.rowids.insert(key.clone(), new_ids);
                }
                // Row positions may have shifted values; drop the PK cache so
                // the next insert rebuilds it from current rows. Refresh the
                // cached rowid maximum from the surviving ids.
                if let Some(ids) = inner.rowids.get(&key) {
                    let max = ids.iter().copied().max().unwrap_or(0);
                    inner.rowid_max.insert(key.clone(), max);
                }
                inner.pk_cache.remove(&key);
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Delete { table, filter } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                let compiled: Option<CompiledWhere> = {
                    let tbl = inner.tables.get(&key).ok_or_else(|| SqlError::SqliteFailure {
                        code: 1,
                        message: format!("no such table: {table}"),
                    })?;
                    let col_names: Vec<String> =
                        tbl.columns.iter().map(|c| c.name.clone()).collect();
                    let map = parser::column_index_map(&col_names);
                    filter
                        .as_ref()
                        .map(|f| f.compile(&map, bound))
                        .transpose()?
                };
                let before = inner.tables.get(&key).ok_or_else(|| missing_table(&key))?.rows.len();
                let old_ids = inner.rowids.get(&key).cloned().unwrap_or_default();
                let mut kept = Vec::with_capacity(before);
                let mut kept_ids = Vec::with_capacity(before);
                {
                    let tbl = inner.tables.get_mut(&key).ok_or_else(|| missing_table(&key))?;
                    for (row, id) in tbl.rows.drain(..).zip(old_ids.into_iter().chain(std::iter::repeat(0))).take(before) {
                        let remove = match &compiled {
                            Some(f) => f.matches_row(&row),
                            None => true,
                        };
                        if !remove {
                            kept.push(row);
                            kept_ids.push(id);
                        }
                    }
                    tbl.rows = kept;
                }
                // Freed pages rejoin the freelist trunk on the next persist
                // (the rebuilt image preserves the file size); rowids of the
                // surviving rows travel with them.
                inner.rowids.insert(key.clone(), kept_ids);
                if let Some(ids) = inner.rowids.get(&key) {
                    let max = ids.iter().copied().max().unwrap_or(0);
                    inner.rowid_max.insert(key.clone(), max);
                }
                inner.pk_cache.remove(&key);
                let changed = before - inner.tables.get(&key).ok_or_else(|| missing_table(&key))?.rows.len();
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Begin => self.begin_inner().map(|()| 0),
            Stmt::Commit => self.commit_inner().map(|()| 0),
            Stmt::Rollback => self.rollback_inner().map(|()| 0),
        }
    }

    pub(crate) fn begin_inner(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.in_transaction {
            return Err(SqlError::Transaction("already in a transaction".into()));
        }
        inner.backup = Some(Backup {
            tables: inner.tables.clone(),
            schemas: inner.schemas.clone(),
            display_names: inner.display_names.clone(),
            rowids: inner.rowids.clone(),
            rowid_max: inner.rowid_max.clone(),
            indexes: inner.indexes.clone(),
        });
        inner.in_transaction = true;
        Ok(())
    }

    pub(crate) fn commit_inner(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if !inner.in_transaction {
            return Err(SqlError::Transaction("no transaction to commit".into()));
        }
        inner.backup = None;
        inner.in_transaction = false;
        Ok(())
    }

    pub(crate) fn rollback_inner(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if !inner.in_transaction {
            return Err(SqlError::Transaction("no transaction to roll back".into()));
        }
        if let Some(backup) = inner.backup.take() {
            inner.tables = backup.tables;
            inner.schemas = backup.schemas;
            inner.display_names = backup.display_names;
            inner.rowids = backup.rowids;
            inner.rowid_max = backup.rowid_max;
            inner.indexes = backup.indexes;
        }
        // Restored rows invalidate cached PK positions.
        inner.pk_cache.clear();
        inner.in_transaction = false;
        inner.changes = 0;
        Ok(())
    }
}

/// Internal invariant failure: the table key was validated against
/// `inner.tables` earlier in the same borrow, so this is unreachable unless
/// a future refactor breaks the check-then-use order. Returned as an error
/// (never a panic) so crafted input can never crash the process.
fn missing_table(key: &str) -> SqlError {
    SqlError::SqliteFailure { code: 1, message: format!("no such table: {key}") }
}

/// Smallest positive rowid not in `used`; `None` when the whole range is
/// exhausted (practically unreachable, only via explicit `i64::MAX`
/// inserts filling the space).
fn smallest_free_rowid(used: &std::collections::HashSet<i64>) -> Option<i64> {
    let mut candidate = 1i64;
    loop {
        if !used.contains(&candidate) {
            return Some(candidate);
        }
        candidate = candidate.checked_add(1)?;
    }
}

pub(crate) fn resolve_expr(expr: &Expr, bound: &[Value]) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Placeholder(n) => bound.get(n - 1).cloned().ok_or(SqlError::InvalidParameterCount {
            expected: *n,
            got: bound.len(),
        }),
    }
}

/// True for statements that change durable state and therefore need a file
/// snapshot afterwards. `Begin` only snapshots memory and `Rollback`
/// restores memory to the last persisted state, so neither touches the file.
pub(crate) fn is_write(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::AlterTable { .. }
            | Stmt::Insert { .. }
            | Stmt::Update { .. }
            | Stmt::Delete { .. }
            | Stmt::Pragma { .. }
            | Stmt::Commit
    )
}

/// Fetch a typed value from a row by index or name (used by `Row::get`).
pub fn column_value<T: FromValue>(columns: &[String], values: &[Value], name: &str) -> Result<T> {
    let idx = columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .ok_or_else(|| SqlError::InvalidColumnName(name.to_owned()))?;
    T::from_value(&values[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragma_batch_is_accepted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )
        .unwrap();
    }

    #[test]
    fn insert_or_replace_overwrites_pk() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", ("a", 1i32)).unwrap();
        conn.execute("INSERT OR REPLACE INTO t (id, v) VALUES (?1, ?2)", ("a", 2i32)).unwrap();
        let v: i32 = conn.query_row("SELECT v FROM t WHERE id = ?1", ("a",), |row| row.get(0)).unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn update_and_delete_use_extended_filters() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, age INTEGER, tag TEXT)")
            .unwrap();
        for (id, age, tag) in [("a", 18i32, "x"), ("b", 30i32, "y"), ("c", 40i32, "z")] {
            conn.execute(
                "INSERT INTO t (id, age, tag) VALUES (?1, ?2, ?3)",
                crate::params![id, age, tag],
            )
            .unwrap();
        }
        let changed = conn
            .execute("UPDATE t SET tag = ?1 WHERE age >= ?2 OR id = ?3", crate::params!["old", 40i32, "a"])
            .unwrap();
        assert_eq!(changed, 2);
        let removed = conn
            .execute("DELETE FROM t WHERE tag NOT LIKE 'o%' AND age IS NOT NULL", ())
            .unwrap();
        assert_eq!(removed, 1);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }

    #[test]
    fn pk_cache_invalidated_by_delete_and_update() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 10i32]).unwrap();
        // Duplicate must fail (populates the PK cache on the first insert).
        let dup = conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 20i32]);
        assert!(dup.is_err());
        // After DELETE the key is reusable.
        conn.execute("DELETE FROM t WHERE id = ?1", crate::params![1i32]).unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 30i32]).unwrap();
        let v: i64 = conn.query_row("SELECT v FROM t WHERE id = ?1", crate::params![1i32], |row| row.get(0)).unwrap();
        assert_eq!(v, 30);
        // After UPDATE of the key the old key is reusable.
        conn.execute("UPDATE t SET id = ?1 WHERE id = ?2", crate::params![2i32, 1i32]).unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 40i32]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn pk_cache_cleared_by_rollback() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 1i32]).unwrap();
        {
            let tx = conn.transaction().unwrap();
            tx.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![2i32, 2i32]).unwrap();
            tx.rollback().unwrap();
        }
        // Row 2 was rolled back: re-inserting it must succeed, and the
        // pre-transaction row must still conflict.
        conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![2i32, 3i32]).unwrap();
        let dup = conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![1i32, 9i32]);
        assert!(dup.is_err());
    }

    #[test]
    fn transaction_defers_file_snapshot_until_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deferred.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        // Native B-Tree files preallocate one root leaf per table (like
        // SQLite), so small inserts may not grow the file; compare bytes to
        // prove nothing landed while the transaction was open.
        let before = std::fs::read(&path).unwrap();
        let tx = conn.transaction().unwrap();
        for i in 0..100i32 {
            tx.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![i, i]).unwrap();
        }
        // No write landed while the transaction was open.
        assert_eq!(std::fs::read(&path).unwrap(), before);
        tx.commit().unwrap();
        // Commit flushed exactly the rows; reload proves durability.
        assert_ne!(std::fs::read(&path).unwrap(), before);
        conn.close().unwrap();
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 100);
    }

    #[test]
    fn prepared_write_persists_file_backed_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stmt.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").unwrap();
            let mut stmt = conn.prepare("INSERT INTO t (id, v) VALUES (?1, ?2)").unwrap();
            stmt.execute(crate::params!["a", 1i32]).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn.query_row("SELECT v FROM t WHERE id = ?1", crate::params!["a"], |row| row.get(0)).unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn statement_cache_reuses_parses_across_executes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
        // Same SQL text three times with different bindings: each call must
        // run (cache hits after the first parse) with its own parameters.
        for i in 0..3i32 {
            conn.execute(
                "INSERT INTO t (id, v) VALUES (?1, ?2)",
                crate::params![i, format!("v{i}")],
            )
            .unwrap();
        }
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 3);
        // Cached statements stay usable through `prepare` and `query_row`.
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id = ?1", crate::params![1i32], |row| row.get(0))
            .unwrap();
        assert_eq!(v, "v1");
        let mut stmt = conn.prepare("INSERT INTO t (id, v) VALUES (?1, ?2)").unwrap();
        stmt.execute(crate::params![9i32, "nine"]).unwrap();
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id = ?1", crate::params![9i32], |row| row.get(0))
            .unwrap();
        assert_eq!(v, "nine");
    }

    #[test]
    fn statement_cache_evicts_without_losing_correctness() {
        let conn = Connection::open_in_memory().unwrap();
        // Push far more distinct statements than the cache holds so the
        // eviction path runs; every statement must still execute correctly
        // and evicted statements must re-parse on reuse.
        for i in 0..300i32 {
            conn.execute_batch(&format!("CREATE TABLE evict_{i} (id INTEGER PRIMARY KEY)")).unwrap();
        }
        for i in 0..300i32 {
            conn.execute(
                &format!("INSERT INTO evict_{i} (id) VALUES (?1)"),
                crate::params![i],
            )
            .unwrap();
        }
        for i in 0..300i32 {
            let v: i64 = conn
                .query_row(&format!("SELECT id FROM evict_{i}"), (), |row| row.get(0))
                .unwrap();
            assert_eq!(v, i as i64);
        }
        // The earliest statement still works after eviction.
        conn.execute("INSERT INTO evict_0 (id) VALUES (?1)", crate::params![999i32]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM evict_0", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn statement_cache_does_not_poison_on_parse_errors() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(conn.execute("INSERT INTO", ()).is_err());
        // A failed parse leaves the cache clean: valid SQL works afterwards
        // and the same invalid SQL keeps failing deterministically.
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        conn.execute("INSERT INTO t (id) VALUES (?1)", crate::params![1i32]).unwrap();
        assert!(conn.execute("INSERT INTO", ()).is_err());
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", (), |row| row.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn select_order_limit_offset_and_in() {        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        for i in 0..10i32 {
            conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![i, i * 10]).unwrap();
        }
        let top: Vec<i64> = conn
            .prepare("SELECT v FROM t WHERE id IN (?1, ?2, ?3) ORDER BY v DESC LIMIT 2")
            .unwrap()
            .query_map(crate::params![1i32, 5i32, 9i32], |row| row.get(0))
            .unwrap()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(top, vec![90i64, 50i64]);
    }
}
