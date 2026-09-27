//! `Statement` plus streaming `Row` / `MappedRows`, ruslite-style.

use crate::connection::Connection;
use crate::error::{Result, SqlError};
use crate::parser::{
    column_index_map, sort_compare, CompiledWhere, Expr, LimitValue, OrderBy, Stmt, WhereClause,
};
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
                let changed = self.conn.run_stmt(&owned, &bound)?;
                // File-backed prepared writes are durable, exactly like
                // `Connection::execute`; deferred while a transaction is open.
                if crate::connection::is_write(&owned) {
                    self.conn.persist_if_needed()?;
                }
                Ok(changed)
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
    /// result sets never fully materialize in RAM. The filter applies while
    /// scanning; `ORDER BY` buffers only the filtered rows for sorting, then
    /// `OFFSET` / `LIMIT` trim the stream. `COUNT(*)` yields exactly one
    /// integer row.
    pub fn query_map<T, P, F>(&mut self, params: P, f: F) -> Result<MappedRows<'conn, F>>
    where
        P: IntoParams,
        F: FnMut(&Row) -> Result<T>,
    {
        let bound = params.into_params()?;
        let (table, columns, star, filter, order_by, limit, offset, count_star) = match &self.stmt {
            Stmt::Select {
                columns,
                star,
                table,
                filter,
                order_by,
                limit,
                offset,
                count_star,
            } => (
                table.clone(),
                columns.clone(),
                *star,
                filter.clone(),
                order_by.clone(),
                limit.clone(),
                offset.clone(),
                *count_star,
            ),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        let limit = resolve_limit_opt(limit.as_ref(), &bound, false)?;
        let offset = resolve_limit_opt(offset.as_ref(), &bound, true)?;
        Ok(MappedRows::new(
            self.conn, table, columns, star, filter, order_by, limit, offset, count_star, bound, f,
        ))
    }

    /// Run a SELECT and collect rows (convenience; prefer `query_map`).
    pub fn query(&mut self, params: impl IntoParams) -> Result<Vec<Row>> {
        let bound = params.into_params()?;
        let (table, columns, star, filter, order_by, limit, offset, count_star) = match &self.stmt {
            Stmt::Select {
                columns,
                star,
                table,
                filter,
                order_by,
                limit,
                offset,
                count_star,
            } => (
                table.clone(),
                columns.clone(),
                *star,
                filter.clone(),
                order_by.clone(),
                limit.clone(),
                offset.clone(),
                *count_star,
            ),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        collect_rows(
            self.conn,
            &table,
            &columns,
            star,
            filter.as_ref(),
            &order_by,
            limit.as_ref(),
            offset.as_ref(),
            count_star,
            &bound,
        )
    }

    pub fn column_count(&self) -> usize {
        match &self.stmt {
            Stmt::Select {
                columns,
                star,
                count_star,
                ..
            } => {
                if *count_star {
                    1
                } else if *star {
                    0
                } else {
                    columns.len()
                }
            }
            _ => 0,
        }
    }
}

/// Query plan compiled once per scan: resolved table key, output columns,
/// projection indices and filter. Per-row work is then pure indexing with no
/// string comparisons and no per-row schema clones. The table rows themselves
/// are still read lazily one at a time, so the streaming guarantee holds.
#[derive(Clone, Debug)]
struct ScanPlan {
    table_key: String,
    ncols: usize,
    out_columns: Vec<String>,
    proj: Option<Vec<usize>>,
    filter: Option<CompiledWhere>,
}

fn build_plan(
    conn: &Connection,
    table: &str,
    requested: &[String],
    star: bool,
    filter: Option<&WhereClause>,
    bound: &[Value],
) -> Result<ScanPlan> {
    let inner = conn.inner.borrow();
    let table_key = table.to_ascii_lowercase();
    let tbl = inner.tables.get(&table_key).ok_or_else(|| SqlError::SqliteFailure {
        code: 1,
        message: format!("no such table: {table}"),
    })?;
    let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
    let map = column_index_map(&table_cols);
    let (out_columns, proj) = if star {
        (table_cols.clone(), None)
    } else {
        let mut idxs = Vec::with_capacity(requested.len());
        let mut names = Vec::with_capacity(requested.len());
        for name in requested {
            let i = map
                .get(&name.to_ascii_lowercase())
                .copied()
                .ok_or_else(|| SqlError::InvalidColumnName(name.clone()))?;
            idxs.push(i);
            names.push(table_cols[i].clone());
        }
        (names, Some(idxs))
    };
    let filter = filter.map(|f| f.compile(&map, bound)).transpose()?;
    Ok(ScanPlan {
        table_key,
        ncols: table_cols.len(),
        out_columns,
        proj,
        filter,
    })
}

fn project_planned(plan: &ScanPlan, row: &[Value]) -> Row {
    match &plan.proj {
        None => Row {
            columns: plan.out_columns.clone(),
            values: row.to_vec(),
        },
        Some(idxs) => Row {
            columns: plan.out_columns.clone(),
            values: idxs.iter().map(|&i| row[i].clone()).collect(),
        },
    }
}

/// Resolve a `LIMIT` / `OFFSET` bound to a row count.
///
/// A negative `LIMIT` means "no upper bound" (SQLite compatible); a negative
/// `OFFSET` clamps to zero.
pub(crate) fn resolve_limit_opt(
    value: Option<&LimitValue>,
    bound: &[Value],
    is_offset: bool,
) -> Result<Option<i64>> {
    match value {
        None => Ok(None),
        Some(LimitValue::Literal(v)) => Ok(normalize_limit(*v, is_offset)),
        Some(LimitValue::Placeholder(n)) => {
            let raw = bound.get(n - 1).ok_or(SqlError::InvalidParameterCount {
                expected: *n,
                got: bound.len(),
            })?;
            let num = raw
                .as_i64()
                .ok_or_else(|| SqlError::FromSqlConversionFailure("LIMIT requires an integer".into()))?;
            Ok(normalize_limit(num, is_offset))
        }
    }
}

fn normalize_limit(value: i64, is_offset: bool) -> Option<i64> {
    if value < 0 {
        if is_offset {
            Some(0)
        } else {
            None
        }
    } else {
        Some(value)
    }
}

fn apply_offset_limit<T>(rows: Vec<T>, offset: Option<i64>, limit: Option<i64>) -> Vec<T> {
    let mut rows = rows;
    let skip = offset.unwrap_or(0).max(0) as usize;
    if skip >= rows.len() {
        return Vec::new();
    }
    rows.drain(..skip);
    if let Some(n) = limit {
        let take = (n.max(0) as usize).min(rows.len());
        rows.truncate(take);
    }
    rows
}

/// Name of the single output column of `SELECT COUNT(*)`.
pub const COUNT_COLUMN: &str = "COUNT(*)";

pub(crate) fn count_row(count: i64) -> Row {
    Row {
        columns: vec![COUNT_COLUMN.to_owned()],
        values: vec![Value::Integer(count)],
    }
}

fn sort_table_rows(table_cols: &[String], rows: &mut [Vec<Value>], order_by: &[OrderBy]) -> Result<()> {
    let mut keys = Vec::with_capacity(order_by.len());
    for key in order_by {
        let idx = table_cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&key.col))
            .ok_or_else(|| SqlError::InvalidColumnName(key.col.clone()))?;
        keys.push((idx, key.desc));
    }
    rows.sort_by(|a, b| {
        for (idx, desc) in &keys {
            let ord = sort_compare(&a[*idx], &b[*idx]);
            if ord != std::cmp::Ordering::Equal {
                return if *desc { ord.reverse() } else { ord };
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(())
}

pub(crate) fn collect_rows(
    conn: &Connection,
    table: &str,
    requested: &[String],
    star: bool,
    filter: Option<&WhereClause>,
    order_by: &[OrderBy],
    limit: Option<&LimitValue>,
    offset: Option<&LimitValue>,
    count_star: bool,
    bound: &[Value],
) -> Result<Vec<Row>> {
    let limit = resolve_limit_opt(limit, bound, false)?;
    let offset = resolve_limit_opt(offset, bound, true)?;
    let plan = build_plan(conn, table, requested, star, filter, bound)?;
    let inner = conn.inner.borrow();
    let tbl = inner.tables.get(&plan.table_key).ok_or_else(|| SqlError::SqliteFailure {
        code: 1,
        message: format!("no such table: {table}"),
    })?;
    if count_star {
        let mut count = 0i64;
        for row in &tbl.rows {
            let keep = match &plan.filter {
                Some(f) => f.matches_row(row),
                None => true,
            };
            if keep {
                count += 1;
            }
        }
        return Ok(vec![count_row(count)]);
    }
    let mut matched: Vec<Vec<Value>> = Vec::new();
    for row in &tbl.rows {
        let keep = match &plan.filter {
            Some(f) => f.matches_row(row),
            None => true,
        };
        if keep {
            matched.push(row.clone());
        }
    }
    if !order_by.is_empty() {
        let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
        sort_table_rows(&table_cols, &mut matched, order_by)?;
    }
    let matched = apply_offset_limit(matched, offset, limit);
    let mut out = Vec::with_capacity(matched.len());
    for row in &matched {
        out.push(project_planned(&plan, row));
    }
    Ok(out)
}

/// Lazy streaming iterator over query results.
///
/// Holds a cursor into the table and evaluates the filter per row, so only
/// one row is materialized at a time. The mapping closure runs per item
/// and its errors surface as `Some(Err(...))`, like ruslite.
///
/// Execution order is filter, then sort, then `OFFSET` / `LIMIT`:
/// without `ORDER BY` the scan stays fully lazy (matching rows are skipped
/// for `OFFSET` and the stream ends after `LIMIT` rows). With `ORDER BY`
/// only the filtered rows are buffered for sorting; the table itself is
/// never copied unfiltered. `COUNT(*)` yields exactly one integer row.
pub struct MappedRows<'conn, F> {
    conn: &'conn Connection,
    table: String,
    requested: Vec<String>,
    star: bool,
    filter: Option<WhereClause>,
    order_by: Vec<OrderBy>,
    limit: Option<i64>,
    offset: Option<i64>,
    count_star: bool,
    bound: Vec<Value>,
    /// Compiled on first `next()` so `query_map` itself keeps its current
    /// behavior (table and column errors surface during iteration, exactly
    /// like the previous per-row resolution).
    plan: Option<ScanPlan>,
    pos: usize,
    skipped: usize,
    emitted: usize,
    count_done: bool,
    sorted_init: bool,
    sorted: Vec<Row>,
    sorted_pos: usize,
    mapper: F,
}

impl<'conn, F> MappedRows<'conn, F> {
    fn new(
        conn: &'conn Connection,
        table: String,
        requested: Vec<String>,
        star: bool,
        filter: Option<WhereClause>,
        order_by: Vec<OrderBy>,
        limit: Option<i64>,
        offset: Option<i64>,
        count_star: bool,
        bound: Vec<Value>,
        mapper: F,
    ) -> Self {
        Self {
            conn,
            table,
            requested,
            star,
            filter,
            order_by,
            limit,
            offset,
            count_star,
            bound,
            plan: None,
            pos: 0,
            skipped: 0,
            emitted: 0,
            count_done: false,
            sorted_init: false,
            sorted: Vec::new(),
            sorted_pos: 0,
            mapper,
        }
    }

    /// Compile the scan plan on first use. Afterwards every row is evaluated
    /// with pure indexing: no table-key hashing beyond the lookup, no column
    /// clones, no string comparisons.
    fn ensure_plan(&mut self) -> Result<()> {
        if self.plan.is_none() {
            let plan = build_plan(
                self.conn,
                &self.table,
                &self.requested,
                self.star,
                self.filter.as_ref(),
                &self.bound,
            )?;
            self.plan = Some(plan);
        }
        Ok(())
    }

    /// Drop a stale plan (column count changed mid-iteration, only possible
    /// through transactional DDL + rollback under the iterator) and rebuild
    /// it from the stored statement parts.
    fn ensure_plan_rebuild(&mut self) -> Result<()> {
        self.plan = None;
        self.ensure_plan()
    }

    fn offset_remaining(&self) -> usize {
        self.offset.unwrap_or(0).max(0) as usize - self.skipped.min(self.offset.unwrap_or(0).max(0) as usize)
    }

    fn limit_reached(&self) -> bool {
        match self.limit {
            Some(n) => self.emitted >= n.max(0) as usize,
            None => false,
        }
    }

    /// Buffer only the filtered rows, sort them, then trim to the
    /// `OFFSET` / `LIMIT` window. Runs once on the first `next()`.
    fn init_sorted(&mut self) -> Result<()> {
        self.ensure_plan()?;
        let (table_cols, full_rows, plan) = {
            let inner = self.conn.inner.borrow();
            // Clone the small plan for use after the borrow ends.
            let plan = self.plan.as_ref().expect("plan ensured").clone();
            let tbl = inner.tables.get(&plan.table_key).ok_or_else(|| {
                SqlError::SqliteFailure {
                    code: 1,
                    message: format!("no such table: {}", self.table),
                }
            })?;
            let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
            let mut full_rows = Vec::new();
            for row in &tbl.rows {
                let keep = match &plan.filter {
                    Some(f) => f.matches_row(row),
                    None => true,
                };
                if keep {
                    full_rows.push(row.clone());
                }
            }
            (table_cols, full_rows, plan)
        };
        let mut full_rows = full_rows;
        sort_table_rows(&table_cols, &mut full_rows, &self.order_by)?;
        let full_rows = apply_offset_limit(full_rows, self.offset, self.limit);
        let mut out = Vec::with_capacity(full_rows.len());
        for row in &full_rows {
            out.push(project_planned(&plan, row));
        }
        self.sorted = out;
        self.sorted_pos = 0;
        self.sorted_init = true;
        Ok(())
    }

    fn next_count(&mut self) -> Option<Result<Row>> {
        if self.count_done {
            return None;
        }
        self.count_done = true;
        if let Err(e) = self.ensure_plan() {
            return Some(Err(e));
        }
        let result = (|| {
            let inner = self.conn.inner.borrow();
            let plan = self.plan.as_ref().expect("plan ensured");
            let tbl = inner.tables.get(&plan.table_key).ok_or_else(|| {
                SqlError::SqliteFailure {
                    code: 1,
                    message: format!("no such table: {}", self.table),
                }
            })?;
            let mut count = 0i64;
            for row in &tbl.rows {
                let keep = match &plan.filter {
                    Some(f) => f.matches_row(row),
                    None => true,
                };
                if keep {
                    count += 1;
                }
            }
            Ok(count_row(count))
        })();
        Some(result)
    }
}

impl<'conn, T, F> Iterator for MappedRows<'conn, F>
where
    F: FnMut(&Row) -> Result<T>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.count_star {
            let row = match self.next_count() {
                Some(Ok(row)) => row,
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
            };
            return Some((self.mapper)(&row));
        }
        if !self.order_by.is_empty() {
            if !self.sorted_init {
                if let Err(e) = self.init_sorted() {
                    self.sorted_init = true;
                    return Some(Err(e));
                }
            }
            if self.sorted_pos >= self.sorted.len() {
                return None;
            }
            let row = &self.sorted[self.sorted_pos];
            self.sorted_pos += 1;
            // Borrow ends before the mapper runs: clone the single row.
            let row = row.clone();
            return Some((self.mapper)(&row));
        }
        if self.limit_reached() {
            return None;
        }
        // Compile the plan on first use so column and filter errors surface
        // during iteration, exactly like the previous per-row resolution.
        if let Err(e) = self.ensure_plan() {
            return Some(Err(e));
        }
        loop {
            let row = {
                let inner = self.conn.inner.borrow();
                // Reborrow the plan per row: the mapping closure runs without
                // any borrow held, so it may write through the connection and
                // only a stale schema (column count) forces a recompile.
                let stale = match self.plan.as_ref() {
                    Some(plan) => match inner.tables.get(&plan.table_key) {
                        Some(tbl) => tbl.columns.len() != plan.ncols,
                        None => false,
                    },
                    None => true,
                };
                if stale {
                    drop(inner);
                    match self.ensure_plan_rebuild() {
                        Ok(()) => continue,
                        Err(e) => return Some(Err(e)),
                    }
                }
                let plan = self.plan.as_ref().expect("plan ensured");
                let tbl = match inner.tables.get(&plan.table_key) {
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
                let row = &tbl.rows[self.pos];
                self.pos += 1;
                let keep = match &plan.filter {
                    Some(f) => f.matches_row(row),
                    None => true,
                };
                if !keep {
                    continue;
                }
                if self.offset_remaining() > 0 {
                    self.skipped += 1;
                    continue;
                }
                if self.limit_reached() {
                    return None;
                }
                project_planned(plan, row)
            };
            self.emitted += 1;
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

    fn sample_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, age INTEGER, name TEXT)")
            .unwrap();
        let rows = [
            ("a", 30i32, "Alice"),
            ("b", 20i32, "Bob"),
            ("c", 25i32, "Carol"),
            ("d", 20i32, "Dave"),
        ];
        for (id, age, name) in rows {
            conn.execute(
                "INSERT INTO t (id, age, name) VALUES (?1, ?2, ?3)",
                params![id, age, name],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn where_comparison_operators() {
        let conn = sample_conn();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age >= ?1")
            .unwrap()
            .query_map(params![25i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Alice".to_string(), "Carol".to_string()]);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age != ?1")
            .unwrap()
            .query_map(params![20i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Alice".to_string(), "Carol".to_string()]);
    }

    #[test]
    fn where_like_in_null_and_or() {
        let conn = sample_conn();
        conn.execute(
            "INSERT INTO t (id, age, name) VALUES (?1, ?2, ?3)",
            params!["e", None::<i32>, "Erin"],
        )
        .unwrap();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE name LIKE 'A%'")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Alice".to_string()]);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age IN (?1, ?2)")
            .unwrap()
            .query_map(params![20i32, 30i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names.len(), 3);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age IS NULL")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Erin".to_string()]);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age = ?1 OR age = ?2")
            .unwrap()
            .query_map(params![30i32, 25i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Alice".to_string(), "Carol".to_string()]);
    }

    #[test]
    fn order_by_limit_offset_streams() {
        let conn = sample_conn();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t ORDER BY age ASC, name DESC LIMIT 2 OFFSET 1")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names, vec!["Bob".to_string(), "Carol".to_string()]);
    }

    #[test]
    fn count_star_returns_single_row() {
        let conn = sample_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE age >= ?1", params![25i32], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(stmt.column_count(), 1);
        let n: i64 = stmt.query_row(params![], |row| row.get(0)).unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn named_params_bind_positionally() {
        let conn = sample_conn();
        let name: String = conn
            .query_row(
                "SELECT name FROM t WHERE age = :age AND id = @id",
                params![20i32, "b"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Bob");
    }
}
