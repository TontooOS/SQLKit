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
    pub indexes: HashMap<String, (String, Vec<String>)>,
    pub pragmas: HashMap<String, String>,
    pub changes: usize,
    pub last_rowid: i64,
    pub in_transaction: bool,
    pub backup: Option<HashMap<String, Table>>,
    /// In-memory primary-key lookup cache: table key to (key value, row
    /// position). Never serialized; rebuilt lazily from `tables` and
    /// invalidated on any `UPDATE`, `DELETE` or rollback. Turns the
    /// per-insert uniqueness scan from O(rows) into O(1).
    pub pk_cache: HashMap<String, HashMap<PkKey, usize>>,
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
            }),
            stmt_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Open (or create) a file-backed database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !pager::file_exists(&path) {
            let conn = Self::fresh(Some(path.clone()));
            pager::create_new(&path)?;
            return Ok(conn);
        }
        let tables = pager::load(&path)?;
        let conn = Self::fresh(Some(path));
        conn.inner.borrow_mut().tables = tables;
        Ok(conn)
    }

    /// Open a transient in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::fresh(None))
    }

    /// Write the file snapshot when it is due. Skipped while a transaction
    /// is open so bulk loads inside `transaction()` produce exactly one
    /// snapshot at commit instead of one per row; the file then always
    /// equals the last committed state. Borrows the tables in place instead
    /// of cloning them first.
    pub(crate) fn persist_if_needed(&self) -> Result<()> {
        if self.inner.borrow().in_transaction {
            return Ok(());
        }
        let inner = self.inner.borrow();
        if let Some(path) = inner.path.clone() {
            pager::save(&path, &inner.tables)?;
        }
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
        self.begin_inner()?;
        Ok(Transaction::new(self))
    }

    /// Number of rows changed by the last write.
    pub fn changes(&self) -> usize {
        self.inner.borrow().changes
    }

    /// Checkpoint the WAL (basis: validates the pragma, no-op otherwise).
    pub fn checkpoint(&self) -> Result<()> {
        self.inner
            .borrow_mut()
            .pragmas
            .insert("wal_checkpoint".into(), "TRUNCATE".into());
        self.persist_if_needed()?;
        Ok(())
    }

    /// Flush and release the handle.
    pub fn close(self) -> Result<()> {
        self.persist_if_needed()?;
        Ok(())
    }

    pub(crate) fn run_stmt(&self, stmt: &Stmt, bound: &[Value]) -> Result<usize> {
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
                inner.tables.insert(name.to_ascii_lowercase(), table);
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
                inner.indexes.insert(
                    name.to_ascii_lowercase(),
                    (table.clone(), columns.clone()),
                );
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
                    let tbl = inner.tables.get(&key).expect("table checked above");
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
                    for row in staged {
                        let lookup = PkKey::of(&row[pk_idx]);
                        let existing = inner
                            .pk_cache
                            .get(&key)
                            .and_then(|m| m.get(&lookup))
                            .copied();
                        if let Some(pos) = existing {
                            if *or_replace {
                                inner
                                    .tables
                                    .get_mut(&key)
                                    .expect("table checked above")
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
                                    inner.tables.get_mut(&key).expect("table checked above");
                                let pos = tbl.rows.len();
                                tbl.rows.push(row);
                                pos
                            };
                            inner
                                .pk_cache
                                .get_mut(&key)
                                .expect("cache just built")
                                .insert(lookup, pos);
                            changed += 1;
                        }
                    }
                } else {
                    let tbl = inner.tables.get_mut(&key).expect("table checked above");
                    changed = staged.len();
                    tbl.rows.extend(staged);
                }
                inner.last_rowid += changed as i64;
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Select { .. } | Stmt::Union { .. } => Err(SqlError::ExecuteReturnedResults),
            Stmt::AlterTable { table, column } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
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
                {
                    let tbl = inner.tables.get_mut(&key).expect("table checked above");
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
                // Row positions may have shifted values; drop the PK cache so
                // the next insert rebuilds it from current rows.
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
                let before = inner.tables.get(&key).expect("table checked above").rows.len();
                let mut kept = Vec::with_capacity(before);
                {
                    let tbl = inner.tables.get_mut(&key).expect("table checked above");
                    for row in tbl.rows.drain(..) {
                        let remove = match &compiled {
                            Some(f) => f.matches_row(&row),
                            None => true,
                        };
                        if !remove {
                            kept.push(row);
                        }
                    }
                    tbl.rows = kept;
                }
                inner.pk_cache.remove(&key);
                let changed = before - inner.tables.get(&key).expect("table checked above").rows.len();
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
        inner.backup = Some(inner.tables.clone());
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
            inner.tables = backup;
        }
        // Restored rows invalidate cached PK positions.
        inner.pk_cache.clear();
        inner.in_transaction = false;
        inner.changes = 0;
        Ok(())
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
        let before = std::fs::metadata(&path).unwrap().len();
        let tx = conn.transaction().unwrap();
        for i in 0..100i32 {
            tx.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", crate::params![i, i]).unwrap();
        }
        // No snapshot landed while the transaction was open.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
        tx.commit().unwrap();
        // Commit flushed exactly the rows; reload proves durability.
        assert!(std::fs::metadata(&path).unwrap().len() > before);
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
