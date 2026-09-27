//! `Statement` plus streaming `Row` / `MappedRows`, ruslite-style.

use crate::connection::Connection;
use crate::error::{Result, SqlError};
use crate::parser::{Expr, Stmt, WhereClause};
use crate::value::{FromValue, IntoParams, Value};

/// A single result row.
#[derive(Clone, Debug)]
pub struct Row {
    pub(crate) columns: Vec<String>,
    pub(crate) values: Vec<Value>,
}

impl Row {
    /// Get a typed column by index (`0`-based) or by name.
    pub fn get<I, T>(&self, idx: I) -> Result<T>
    where
        I: ColumnIndex,
        T: FromValue,
    {
        idx.get(&self.columns, &self.values)
    }

    pub fn column_names(&self) -> &[String] {
        &self.columns
    }
}

/// Index into a row, by position or by column name.
pub trait ColumnIndex {
    fn get<T: FromValue>(self, columns: &[String], values: &[Value]) -> Result<T>;
}

impl ColumnIndex for usize {
    fn get<T: FromValue>(self, _columns: &[String], values: &[Value]) -> Result<T> {
        let value = values.get(self).ok_or(SqlError::InvalidColumnIndex(self))?;
        T::from_value(value)
    }
}

impl ColumnIndex for &str {
    fn get<T: FromValue>(self, columns: &[String], values: &[Value]) -> Result<T> {
        let idx = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(self))
            .ok_or_else(|| SqlError::InvalidColumnName(self.to_owned()))?;
        T::from_value(&values[idx])
    }
}

impl ColumnIndex for String {
    fn get<T: FromValue>(self, columns: &[String], values: &[Value]) -> Result<T> {
        <&str as ColumnIndex>::get(self.as_str(), columns, values)
    }
}

/// A prepared statement bound to its parent connection.
pub struct Statement<'conn> {
    conn: &'conn Connection,
    stmt: Stmt,
}

impl<'conn> Statement<'conn> {
    pub(crate) fn new(conn: &'conn Connection, stmt: Stmt) -> Self {
        Self { conn, stmt }
    }

    /// Execute a write statement; errors with `ExecuteReturnedResults` on SELECT.
    pub fn execute(&mut self, params: impl IntoParams) -> Result<usize> {
        let bound = params.into_params()?;
        match &self.stmt {
            Stmt::Select { .. } => Err(SqlError::ExecuteReturnedResults),
            other => {
                let owned = other.clone();
                self.conn.run_stmt(&owned, &bound)
            }
        }
    }

    /// Run a SELECT and map the first row; `QueryReturnedNoRows` when empty.
    pub fn query_row<T, P, F>(&mut self, params: P, f: F) -> Result<T>
    where
        P: IntoParams,
        F: FnOnce(&Row) -> Result<T>,
    {
        let rows = self.query(params)?;
        rows.into_iter().next().ok_or(SqlError::QueryReturnedNoRows).and_then(|row| f(&row))
    }

    /// Run a SELECT and return a lazy iterator over mapped rows.
    ///
    /// Rows stream one at a time (no intermediate `Vec<Row>`), so large
    /// result sets never fully materialize in RAM.
    pub fn query_map<T, P, F>(&mut self, params: P, f: F) -> Result<MappedRows<'conn, F>>
    where
        P: IntoParams,
        F: FnMut(&Row) -> Result<T>,
    {
        let bound = params.into_params()?;
        let (table, columns, star, filter) = match &self.stmt {
            Stmt::Select {
                columns,
                star,
                table,
                filter,
            } => (table.clone(), columns.clone(), *star, filter.clone()),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        Ok(MappedRows::new(self.conn, table, columns, star, filter, bound, f))
    }

    /// Run a SELECT and collect rows (convenience; prefer `query_map`).
    pub fn query(&mut self, params: impl IntoParams) -> Result<Vec<Row>> {
        let bound = params.into_params()?;
        let (table, columns, star, filter) = match &self.stmt {
            Stmt::Select {
                columns,
                star,
                table,
                filter,
            } => (table.clone(), columns.clone(), *star, filter.clone()),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        collect_rows(self.conn, &table, &columns, star, filter.as_ref(), &bound)
    }

    pub fn column_count(&self) -> usize {
        match &self.stmt {
            Stmt::Select { columns, star, .. } => {
                if *star {
                    0
                } else {
                    columns.len()
                }
            }
            _ => 0,
        }
    }
}

pub(crate) fn project_row(
    table_cols: &[String],
    table_row: &[Value],
    requested: &[String],
    star: bool,
) -> Result<(Vec<String>, Vec<Value>)> {
    if star {
        return Ok((table_cols.to_vec(), table_row.to_vec()));
    }
    let mut columns = Vec::with_capacity(requested.len());
    let mut values = Vec::with_capacity(requested.len());
    for name in requested {
        let idx = table_cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .ok_or_else(|| SqlError::InvalidColumnName(name.clone()))?;
        columns.push(table_cols[idx].clone());
        values.push(table_row[idx].clone());
    }
    Ok((columns, values))
}

pub(crate) fn collect_rows(
    conn: &Connection,
    table: &str,
    requested: &[String],
    star: bool,
    filter: Option<&WhereClause>,
    bound: &[Value],
) -> Result<Vec<Row>> {
    let inner = conn.inner.borrow();
    let tbl = inner.tables.get(&table.to_ascii_lowercase()).ok_or_else(|| {
        SqlError::SqliteFailure {
            code: 1,
            message: format!("no such table: {table}"),
        }
    })?;
    let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
    let mut out = Vec::new();
    for row in &tbl.rows {
        let keep = match filter {
            Some(f) => f.matches(&table_cols, row, bound)?,
            None => true,
        };
        if keep {
            let (columns, values) = project_row(&table_cols, row, requested, star)?;
            out.push(Row { columns, values });
        }
    }
    Ok(out)
}

/// Lazy streaming iterator over query results.
///
/// Holds a cursor into the table and evaluates the filter per row, so only
/// one row is materialized at a time. The mapping closure runs per item
/// and its errors surface as `Some(Err(...))`, like ruslite.
pub struct MappedRows<'conn, F> {
    conn: &'conn Connection,
    table: String,
    requested: Vec<String>,
    star: bool,
    filter: Option<WhereClause>,
    bound: Vec<Value>,
    pos: usize,
    mapper: F,
}

impl<'conn, F> MappedRows<'conn, F> {
    fn new(
        conn: &'conn Connection,
        table: String,
        requested: Vec<String>,
        star: bool,
        filter: Option<WhereClause>,
        bound: Vec<Value>,
        mapper: F,
    ) -> Self {
        Self {
            conn,
            table,
            requested,
            star,
            filter,
            bound,
            pos: 0,
            mapper,
        }
    }
}

impl<'conn, T, F> Iterator for MappedRows<'conn, F>
where
    F: FnMut(&Row) -> Result<T>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (columns, values) = {
                let inner = self.conn.inner.borrow();
                let tbl = match inner.tables.get(&self.table.to_ascii_lowercase()) {
                    Some(tbl) => tbl,
                    None => {
                        return Some(Err(SqlError::SqliteFailure {
                            code: 1,
                            message: format!("no such table: {}", self.table),
                        }))
                    }
                };
                if self.pos >= tbl.rows.len() {
                    return None;
                }
                let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
                let row = &tbl.rows[self.pos];
                self.pos += 1;
                let keep = match &self.filter {
                    Some(f) => match f.matches(&table_cols, row, &self.bound) {
                        Ok(keep) => keep,
                        Err(e) => return Some(Err(e)),
                    },
                    None => true,
                };
                if !keep {
                    continue;
                }
                match project_row(&table_cols, row, &self.requested, self.star) {
                    Ok(projected) => projected,
                    Err(e) => return Some(Err(e)),
                }
            };
            let row = Row { columns, values };
            return Some((self.mapper)(&row));
        }
    }
}

/// Resolve an expression against bound parameters (shared with updates).
pub fn resolve_placeholder(expr: &Expr, bound: &[Value]) -> Result<Value> {
    crate::connection::resolve_expr(expr, bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params;

    #[test]
    fn query_map_streams_without_collecting() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE u (id TEXT PRIMARY KEY, age INTEGER)").unwrap();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO u (id, age) VALUES (?1, ?2)",
                params![format!("id{i}"), i as i32],
            )
            .unwrap();
        }
        let mut stmt = conn.prepare("SELECT id FROM u WHERE age = ?1").unwrap();
        let rows: Vec<String> = stmt
            .query_map(params![2i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, vec!["id2".to_string()]);
    }

    #[test]
    fn query_row_no_rows_errors() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
        let mut stmt = conn.prepare("SELECT id FROM t WHERE id = ?1").unwrap();
        let res: Result<String> = stmt.query_row(params!["missing"], |row| row.get(0));
        assert!(matches!(res, Err(SqlError::QueryReturnedNoRows)));
    }
}
