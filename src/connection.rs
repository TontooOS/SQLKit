//! `Connection`: ruslite-compatible database handle over the basis engine.

use crate::error::{Result, SqlError};
use crate::pager;
use crate::parser::{self, ColumnDef, Expr, Stmt};
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
}

impl From<&ColumnDef> for Column {
    fn from(def: &ColumnDef) -> Self {
        Self {
            name: def.name.clone(),
            coltype: def.coltype.clone(),
            primary_key: def.primary_key,
            not_null: def.not_null,
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
}

/// Database connection. Mirrors the `rusqlite::Connection` subset used by
/// CoreData: `open`, `execute`, `execute_batch`, `prepare`, `transaction`.
pub struct Connection {
    pub(crate) inner: RefCell<Inner>,
}

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
            }),
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
        match pager::load(&path) {
            Ok(tables) => {
                let conn = Self::fresh(Some(path));
                conn.inner.borrow_mut().tables = tables;
                Ok(conn)
            }
            Err(SqlError::Unsupported(_)) => Err(SqlError::unsupported(
                "foreign SQLite B-Tree file detected; full pager interop is roadmap (see wiki/Pager.md)",
            )),
            Err(other) => Err(other),
        }
    }

    /// Open a transient in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::fresh(None))
    }

    fn persist_if_needed(&self) -> Result<()> {
        let path = self.inner.borrow().path.clone();
        if let Some(path) = path {
            let tables = self.inner.borrow().tables.clone();
            pager::save(&path, &tables)?;
        }
        Ok(())
    }

    /// Execute a single statement with bound parameters.
    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        let bound = params.into_params()?;
        let stmt = parser::parse_one(sql)?;
        let changes = self.run_stmt(&stmt, &bound)?;
        if is_write(&stmt) {
            self.persist_if_needed()?;
        }
        Ok(changes)
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
        let stmt = parser::parse_one(sql)?;
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
                let tbl = inner.tables.get_mut(&key).ok_or_else(|| SqlError::SqliteFailure {
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
                let pk = tbl.primary_key_index();
                let mut changed = 0;
                for exprs in rows {
                    if exprs.len() != target.len() {
                        return Err(SqlError::InvalidParameterCount {
                            expected: target.len(),
                            got: exprs.len(),
                        });
                    }
                    let mut row = vec![Value::Null; tbl.columns.len()];
                    for (slot, expr) in target.iter().zip(exprs.iter()) {
                        row[*slot] = resolve_expr(expr, bound)?;
                    }
                    if let Some(pk_idx) = pk {
                        if let Some(existing) = tbl.rows.iter().position(|r| r[pk_idx] == row[pk_idx]) {
                            if *or_replace {
                                tbl.rows[existing] = row;
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
                            tbl.rows.push(row);
                            changed += 1;
                        }
                    } else {
                        tbl.rows.push(row);
                        changed += 1;
                    }
                }
                inner.last_rowid += changed as i64;
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Select { .. } => Err(SqlError::ExecuteReturnedResults),
            Stmt::Update {
                table,
                assignments,
                filter,
            } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                let tbl = inner.tables.get_mut(&key).ok_or_else(|| SqlError::SqliteFailure {
                    code: 1,
                    message: format!("no such table: {table}"),
                })?;
                let col_names: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
                let slots: Vec<(usize, Expr)> = assignments
                    .iter()
                    .map(|(col, expr)| {
                        tbl.column_index(col)
                            .map(|idx| (idx, expr.clone()))
                            .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut changed = 0;
                for row in tbl.rows.iter_mut() {
                    let keep = match filter {
                        Some(f) => f.matches(&col_names, row, bound)?,
                        None => true,
                    };
                    if keep {
                        for (slot, expr) in &slots {
                            row[*slot] = resolve_expr(expr, bound)?;
                        }
                        changed += 1;
                    }
                }
                inner.changes = changed;
                Ok(changed)
            }
            Stmt::Delete { table, filter } => {
                let mut inner = self.inner.borrow_mut();
                let key = table.to_ascii_lowercase();
                let tbl = inner.tables.get_mut(&key).ok_or_else(|| SqlError::SqliteFailure {
                    code: 1,
                    message: format!("no such table: {table}"),
                })?;
                let col_names: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
                let before = tbl.rows.len();
                let mut kept = Vec::with_capacity(before);
                for row in tbl.rows.drain(..) {
                    let remove = match filter {
                        Some(f) => f.matches(&col_names, &row, bound)?,
                        None => true,
                    };
                    if !remove {
                        kept.push(row);
                    }
                }
                tbl.rows = kept;
                let changed = before - tbl.rows.len();
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

fn is_write(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::Insert { .. }
            | Stmt::Update { .. }
            | Stmt::Delete { .. }
            | Stmt::Pragma { .. }
            | Stmt::Begin
            | Stmt::Commit
            | Stmt::Rollback
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
    fn select_order_limit_offset_and_in() {
        let conn = Connection::open_in_memory().unwrap();
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
