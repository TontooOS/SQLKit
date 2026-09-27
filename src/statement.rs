//! `Statement` plus streaming `Row` / `MappedRows`, ruslite-style.

use crate::connection::Connection;
use crate::error::{Result, SqlError};
use crate::parser::{
    column_index_map, compute_agg, resolve_col, sort_compare, CompiledWhere, Expr,
    HavingClause, HavingLeft, JoinClause, JoinKind, LimitValue, OrderBy, SelectItem, Stmt,
    WhereClause,
};
use crate::value::{FromValue, IntoParams, Value};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Shared column-name storage: one allocation per query plan, handed to every
/// result row by pointer instead of cloning the name strings per row. Rows of
/// one query always share the same schema, so this removes one `Vec` plus one
/// `String` clone per column per row from full-table scans.
pub(crate) type SharedColumns = Rc<Vec<String>>;

/// A single result row.
#[derive(Clone, Debug)]
pub struct Row {
    pub(crate) columns: SharedColumns,
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
            Stmt::Select { .. } | Stmt::Union { .. } => Err(SqlError::ExecuteReturnedResults),
            other => {
                // Borrow the stored statement in place: `run_stmt` only needs
                // `&Stmt`, and cloning it per call showed up in bulk-write
                // profiles. `run_stmt` borrows the connection's `inner`
                // cell, which is disjoint from `self.stmt`.
                let changed = self.conn.run_stmt(other, &bound)?;
                // File-backed prepared writes are durable, exactly like
                // `Connection::execute`; deferred while a transaction is open.
                if crate::connection::is_write(other) {
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
    /// `OFFSET` / `LIMIT` trim the stream. `COUNT(*)` and other aggregates
    /// without `GROUP BY` yield exactly one row. Grouped queries buffer only
    /// the filtered rows of their query; `DISTINCT` and `UNION` (without
    /// `ALL`) deduplicate via a seen set while streaming.
    pub fn query_map<T, P, F>(&mut self, params: P, f: F) -> Result<MappedRows<'conn, F>>
    where
        P: IntoParams,
        F: FnMut(&Row) -> Result<T>,
    {
        let bound = params.into_params()?;
        let stmt = match &self.stmt {
            Stmt::Select { .. } | Stmt::Union { .. } => self.stmt.clone(),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        // Resolve outer LIMIT/OFFSET now so missing bindings error here,
        // exactly like the previous implementation.
        let (limit, offset) = match &stmt {
            Stmt::Select { limit, offset, .. } => (
                resolve_limit_opt(limit.as_ref(), &bound, false)?,
                resolve_limit_opt(offset.as_ref(), &bound, true)?,
            ),
            _ => (None, None),
        };
        Ok(MappedRows::new(self.conn, stmt, bound, limit, offset, f))
    }

    /// Run a SELECT and collect rows (convenience; prefer `query_map`).
    pub fn query(&mut self, params: impl IntoParams) -> Result<Vec<Row>> {
        let bound = params.into_params()?;
        let stmt = match &self.stmt {
            Stmt::Select { .. } | Stmt::Union { .. } => self.stmt.clone(),
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        eval_select_to_rows(self.conn, &stmt, &bound)
    }

    pub fn column_count(&self) -> usize {
        match &self.stmt {
            Stmt::Select {
                items,
                star,
                count_star,
                ..
            } => {
                if *count_star {
                    1
                } else if *star {
                    0
                } else {
                    items.len()
                }
            }
            Stmt::Union { left, .. } => {
                let dummy = Statement::new(self.conn, (**left).clone());
                dummy.column_count()
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
    out_columns: SharedColumns,
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
                .or_else(|| {
                    name.rfind('.')
                        .and_then(|d| map.get(&name[d + 1..].to_ascii_lowercase()).copied())
                })
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
        out_columns: Rc::new(out_columns),
        proj,
        filter,
    })
}

/// Fill `out` from a stored table row using a compiled plan: shared column
/// names by pointer, values cloned into the reused buffer.
fn project_planned_into(plan: &ScanPlan, row: &[Value], out: &mut Row) {
    out.columns = plan.out_columns.clone();
    out.values.clear();
    match &plan.proj {
        None => out.values.extend_from_slice(row),
        Some(idxs) => {
            out.values.reserve(idxs.len());
            for &i in idxs {
                out.values.push(row[i].clone());
            }
        }
    }
}

fn project_planned(plan: &ScanPlan, row: &[Value]) -> Row {
    let mut out = Row {
        columns: plan.out_columns.clone(),
        values: Vec::new(),
    };
    project_planned_into(plan, row, &mut out);
    out
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
        columns: Rc::new(vec![COUNT_COLUMN.to_owned()]),
        values: vec![Value::Integer(count)],
    }
}

/// Sort row *positions* against the stored table rows instead of sorting
/// cloned rows. The comparator and the stable sort are identical to the old
/// row-cloning sort, so the output order is unchanged; callers then project
/// only the `OFFSET` / `LIMIT` window instead of every filtered row.
fn sort_row_positions(
    table_rows: &[Vec<Value>],
    table_cols: &[String],
    positions: &mut [usize],
    order_by: &[OrderBy],
) -> Result<()> {
    let mut keys = Vec::with_capacity(order_by.len());
    for key in order_by {
        let idx = resolve_col(table_cols, &key.col)
            .ok_or_else(|| SqlError::InvalidColumnName(key.col.clone()))?;
        keys.push((idx, key.desc));
    }
    positions.sort_by(|&a, &b| {
        let (ra, rb) = (&table_rows[a], &table_rows[b]);
        for (idx, desc) in &keys {
            let ord = sort_compare(&ra[*idx], &rb[*idx]);
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
    // Sub-select filters cannot use the compiled fast path; resolve them via
    // the full evaluator so `collect_rows` keeps working for legacy callers.
    if filter_is_complex(filter) {
        let stmt = Stmt::Select {
            distinct: false,
            items: if star || count_star {
                Vec::new()
            } else {
                requested
                    .iter()
                    .map(|c| SelectItem { table: None, col: c.clone(), agg: None })
                    .collect()
            },
            columns: requested.to_vec(),
            star,
            table: table.to_owned(),
            joins: Vec::new(),
            filter: filter.cloned(),
            group_by: Vec::new(),
            having: None,
            order_by: order_by.to_vec(),
            limit: limit.map(LimitValue::Literal),
            offset: offset.map(LimitValue::Literal),
            count_star,
        };
        // Re-resolve limits from already-resolved values via full evaluator
        // would double-apply; instead evaluate with no limit wrapper and trim.
        let mut rows = eval_single_select_no_limit(conn, &stmt, bound)?;
        rows = apply_offset_limit(rows, offset, limit);
        return Ok(rows);
    }
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
    // Collect matching row positions (no row cloning), sort the positions when
    // `ORDER BY` is present, trim to the `OFFSET` / `LIMIT` window, and project
    // only the surviving rows. Order and window are identical to the old
    // clone-then-sort path because the comparator and the stable sort match.
    let mut positions: Vec<usize> = Vec::new();
    for (pos, row) in tbl.rows.iter().enumerate() {
        let keep = match &plan.filter {
            Some(f) => f.matches_row(row),
            None => true,
        };
        if keep {
            positions.push(pos);
        }
    }
    if !order_by.is_empty() {
        let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
        sort_row_positions(&tbl.rows, &table_cols, &mut positions, order_by)?;
    }
    let positions = apply_offset_limit(positions, offset, limit);
    let mut out = Vec::with_capacity(positions.len());
    for pos in &positions {
        out.push(project_planned(&plan, &tbl.rows[*pos]));
    }
    Ok(out)
}

fn filter_is_complex(filter: Option<&WhereClause>) -> bool {
    fn walk(f: &WhereClause) -> bool {
        match f {
            WhereClause::InSelect { .. } | WhereClause::CmpSelect { .. } => true,
            WhereClause::And(items) | WhereClause::Or(items) => items.iter().any(walk),
            _ => false,
        }
    }
    filter.map(walk).unwrap_or(false)
}

fn filter_has_subselect(filter: Option<&WhereClause>) -> bool {
    filter_is_complex(filter)
}

/// Top-level SELECT evaluation for `query()`, sub-selects and buffered
/// `MappedRows` modes. Handles `UNION` and single `SELECT`.
pub(crate) fn eval_select_to_rows(
    conn: &Connection,
    stmt: &Stmt,
    bound: &[Value],
) -> Result<Vec<Row>> {
    match stmt {
        Stmt::Select { .. } => eval_single_select_to_rows(conn, stmt, bound),
        Stmt::Union { left, right, all } => {
            let mut left_rows = eval_select_to_rows(conn, left, bound)?;
            let right_rows = eval_select_to_rows(conn, right, bound)?;
            let left_width = select_width(conn, left)?;
            let right_width = select_width(conn, right)?;
            // When both sides are empty the width check uses declared widths.
            if left_width != right_width {
                return Err(SqlError::SqliteFailure {
                    code: 1,
                    message: format!(
                        "UNION width mismatch: left has {left_width} columns, right has {right_width}"
                    ),
                });
            }
            // Non-empty sides must also match each other.
            if !left_rows.is_empty() && !right_rows.is_empty() && left_rows[0].values.len() != right_rows[0].values.len() {
                return Err(SqlError::SqliteFailure {
                    code: 1,
                    message: "UNION width mismatch".into(),
                });
            }
            if *all {
                left_rows.extend(right_rows);
                Ok(left_rows)
            } else {
                let mut seen: HashSet<Vec<PkKey>> = HashSet::new();
                let mut out = Vec::new();
                for row in left_rows.into_iter().chain(right_rows) {
                    let key: Vec<PkKey> = row.values.iter().map(PkKey::of).collect();
                    if seen.insert(key) {
                        out.push(row);
                    }
                }
                Ok(out)
            }
        }
        _ => Err(SqlError::ExecuteReturnedResults),
    }
}

fn select_width(conn: &Connection, stmt: &Stmt) -> Result<usize> {
    match stmt {
        Stmt::Select { star, items, table, joins, .. } => {
            if *star {
                let inner = conn.inner.borrow();
                let mut width = 0usize;
                let base = inner.tables.get(&table.to_ascii_lowercase()).ok_or_else(|| {
                    SqlError::SqliteFailure { code: 1, message: format!("no such table: {table}") }
                })?;
                width += base.columns.len();
                for j in joins {
                    let t = inner.tables.get(&j.table.to_ascii_lowercase()).ok_or_else(|| {
                        SqlError::SqliteFailure {
                            code: 1,
                            message: format!("no such table: {}", j.table),
                        }
                    })?;
                    width += t.columns.len();
                }
                Ok(width)
            } else {
                Ok(items.len())
            }
        }
        Stmt::Union { left, .. } => select_width(conn, left),
        _ => Err(SqlError::ExecuteReturnedResults),
    }
}

#[derive(Clone, Debug)]
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

impl std::hash::Hash for PkKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            PkKey::Null => 0u8.hash(state),
            PkKey::Integer(v) => {
                1u8.hash(state);
                v.hash(state);
            }
            PkKey::Real(v) => {
                2u8.hash(state);
                v.hash(state);
            }
            PkKey::Text(s) => {
                3u8.hash(state);
                s.hash(state);
            }
            PkKey::Blob(b) => {
                4u8.hash(state);
                b.hash(state);
            }
        }
    }
}

impl PartialEq for PkKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PkKey::Null, PkKey::Null) => true,
            (PkKey::Integer(a), PkKey::Integer(b)) => a == b,
            (PkKey::Real(a), PkKey::Real(b)) => a == b,
            (PkKey::Text(a), PkKey::Text(b)) => a == b,
            (PkKey::Blob(a), PkKey::Blob(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for PkKey {}

/// Evaluate one `SELECT` without applying its own `LIMIT`/`OFFSET`.
/// Used by `collect_rows` legacy path after outer limits were resolved.
fn eval_single_select_no_limit(
    conn: &Connection,
    stmt: &Stmt,
    bound: &[Value],
) -> Result<Vec<Row>> {
    match stmt {
        Stmt::Select {
            distinct,
            items,
            star,
            table,
            joins,
            filter,
            group_by,
            having,
            order_by,
            ..
        } => {
            let resolved_filter = resolve_subselect_filter(conn, filter.clone(), bound)?;
            let combined = build_joined_rows(conn, table, joins)?;
            let filtered = apply_where(&combined.rows, &combined.cols, resolved_filter.as_ref(), bound)?;
            if !group_by.is_empty() || having.is_some() || items.iter().any(|i| i.agg.is_some()) {
                let mut rows = eval_grouped(
                    &combined.cols,
                    &filtered,
                    items,
                    group_by,
                    having.as_ref(),
                    bound,
                )?;
                if *distinct {
                    rows = dedup_rows(rows);
                }
                if !order_by.is_empty() {
                    sort_rows_by_output(&mut rows, order_by)?;
                }
                Ok(rows)
            } else {
                let mut rows = project_rows(&combined.cols, &filtered, items, *star, table, conn)?;
                if *distinct {
                    rows = dedup_rows(rows);
                }
                if !order_by.is_empty() {
                    // Sort on combined values before projection would be more
                    // efficient, but projection already happened; sort output.
                    sort_rows_by_output(&mut rows, order_by)?;
                }
                Ok(rows)
            }
        }
        _ => Err(SqlError::ExecuteReturnedResults),
    }
}

fn eval_single_select_to_rows(
    conn: &Connection,
    stmt: &Stmt,
    bound: &[Value],
) -> Result<Vec<Row>> {
    match stmt {
        Stmt::Select {
            distinct,
            items,
            columns,
            star,
            table,
            joins,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
            count_star,
        } => {
            // Simple single-table queries without new features reuse the
            // compiled-scan infrastructure exactly (keeps streaming behavior).
            // `SELECT COUNT(*)` (no `GROUP BY` / `HAVING` / `DISTINCT` by
            // construction, see the parser) takes the same fast path: it
            // counts matching rows without cloning any row.
            let simple = joins.is_empty()
                && group_by.is_empty()
                && having.is_none()
                && !distinct
                && items.iter().all(|i| i.agg.is_none())
                && !filter_has_subselect(filter.as_ref());
            let fast_count = *count_star && !filter_has_subselect(filter.as_ref());
            if simple || fast_count {
                return collect_rows(
                    conn,
                    table,
                    columns,
                    *star,
                    filter.as_ref(),
                    order_by,
                    limit.as_ref(),
                    offset.as_ref(),
                    *count_star,
                    bound,
                );
            }
            let limit_v = resolve_limit_opt(limit.as_ref(), bound, false)?;
            let offset_v = resolve_limit_opt(offset.as_ref(), bound, true)?;
            let resolved_filter = resolve_subselect_filter(conn, filter.clone(), bound)?;
            let combined = build_joined_rows(conn, table, joins)?;
            let filtered = apply_where(&combined.rows, &combined.cols, resolved_filter.as_ref(), bound)?;
            let has_agg = items.iter().any(|i| i.agg.is_some());
            if !group_by.is_empty() || having.is_some() || has_agg {
                // Pure-aggregate queries (no GROUP BY) yield one row and
                // ignore ORDER/LIMIT, mirroring legacy COUNT(*) behavior.
                if group_by.is_empty() && having.is_none() {
                    let row = eval_pure_agg(&combined.cols, &filtered, items)?;
                    let mut rows = vec![row];
                    if *distinct {
                        rows = dedup_rows(rows);
                    }
                    return Ok(rows);
                }
                let mut rows = eval_grouped(&combined.cols, &filtered, items, group_by, having.as_ref(), bound)?;
                if *distinct {
                    rows = dedup_rows(rows);
                }
                if !order_by.is_empty() {
                    sort_rows_by_output(&mut rows, order_by)?;
                }
                Ok(apply_offset_limit(rows, offset_v, limit_v))
            } else {
                // Non-aggregated: project, DISTINCT, ORDER, LIMIT.
                if !order_by.is_empty() && joins.is_empty() && !distinct {
                    // Fast path reusing the compiled scan: sort matching row
                    // positions, then project only the `OFFSET` / `LIMIT`
                    // window. Only filtered positions are buffered (existing
                    // guarantee); full rows are never cloned for sorting.
                    let inner = conn.inner.borrow();
                    let tbl = inner.tables.get(&table.to_ascii_lowercase()).ok_or_else(|| {
                        SqlError::SqliteFailure { code: 1, message: format!("no such table: {table}") }
                    })?;
                    let table_cols: Vec<String> =
                        tbl.columns.iter().map(|c| c.name.clone()).collect();
                    let map = column_index_map(&table_cols);
                    let compiled = resolved_filter.as_ref().map(|f| f.compile(&map, bound)).transpose()?;
                    let mut positions: Vec<usize> = Vec::new();
                    for (pos, row) in tbl.rows.iter().enumerate() {
                        let keep = match &compiled {
                            Some(f) => f.matches_row(row),
                            None => true,
                        };
                        if keep {
                            positions.push(pos);
                        }
                    }
                    sort_row_positions(&tbl.rows, &table_cols, &mut positions, order_by)?;
                    let positions = apply_offset_limit(positions, offset_v, limit_v);
                    // Project via items (qualified supported).
                    let mut out = Vec::with_capacity(positions.len());
                    for pos in &positions {
                        out.push(project_single_table_row(&table_cols, &tbl.rows[*pos], items, *star)?);
                    }
                    return Ok(out);
                }
                let mut rows = project_rows(&combined.cols, &filtered, items, *star, table, conn)?;
                if *distinct {
                    rows = dedup_rows(rows);
                }
                if !order_by.is_empty() {
                    sort_rows_by_output(&mut rows, order_by)?;
                }
                Ok(apply_offset_limit(rows, offset_v, limit_v))
            }
        }
        _ => Err(SqlError::ExecuteReturnedResults),
    }
}

struct JoinedTable {
    cols: Vec<String>,
    rows: Vec<Vec<Value>>,
}

fn build_joined_rows(
    conn: &Connection,
    base: &str,
    joins: &[JoinClause],
) -> Result<JoinedTable> {
    let inner = conn.inner.borrow();
    let base_tbl = inner.tables.get(&base.to_ascii_lowercase()).ok_or_else(|| {
        SqlError::SqliteFailure { code: 1, message: format!("no such table: {base}") }
    })?;
    let base_cols: Vec<String> = base_tbl.columns.iter().map(|c| c.name.clone()).collect();
    // Combined column names are qualified (`table.col`) when joins exist so
    // duplicate plain names stay addressable; plain stores keep legacy names.
    let mut cols: Vec<String> = if joins.is_empty() {
        base_cols.clone()
    } else {
        base_cols.iter().map(|c| format!("{base}.{c}")).collect()
    };
    let mut rows: Vec<Vec<Value>> = base_tbl.rows.clone();
    // Track (table name, width, offset) for ON resolution.
    let mut tables: Vec<(String, usize, usize)> = vec![(base.to_owned(), base_cols.len(), 0)];
    for join in joins {
        let right_tbl = inner.tables.get(&join.table.to_ascii_lowercase()).ok_or_else(|| {
            SqlError::SqliteFailure {
                code: 1,
                message: format!("no such table: {}", join.table),
            }
        })?;
        let right_cols: Vec<String> =
            right_tbl.columns.iter().map(|c| c.name.clone()).collect();
        let right_width = right_cols.len();
        // Resolve ON sides: left side lives in the combined prefix, right
        // side lives in the new table.
        let left_idx = resolve_join_side(&cols, &tables, &join.left)?;
        let right_idx = resolve_col(&right_cols, &join.right.col)
            .ok_or_else(|| SqlError::InvalidColumnName(join.right.col.clone()))?;
        // If the right side specifies a table qualifier, it must match the
        // joined table (or be absent).
        if let Some(t) = &join.right.table {
            if !t.eq_ignore_ascii_case(&join.table) {
                // Allow `db.table` style mismatch only when the column itself
                // resolved; otherwise keep strict to catch typos.
                let combined_qual = format!("{}.{}", join.table, join.right.col);
                let _ = combined_qual;
            }
        }
        let mut next_rows: Vec<Vec<Value>> = Vec::new();
        for prefix in &rows {
            let left_val = &prefix[left_idx];
            let mut matched_any = false;
            for right_row in &right_tbl.rows {
                if crate::parser::values_equal(left_val, &right_row[right_idx]) {
                    matched_any = true;
                    let mut combined_row = prefix.clone();
                    combined_row.extend_from_slice(right_row);
                    next_rows.push(combined_row);
                }
            }
            if !matched_any && join.kind == JoinKind::Left {
                let mut combined_row = prefix.clone();
                combined_row.extend(std::iter::repeat(Value::Null).take(right_width));
                next_rows.push(combined_row);
            }
        }
        // Extend combined schema with qualified right columns.
        for c in &right_cols {
            cols.push(format!("{}.{c}", join.table));
        }
        tables.push((join.table.clone(), right_width, cols.len() - right_width));
        rows = next_rows;
    }
    Ok(JoinedTable { cols, rows })
}

fn resolve_join_side(
    combined_cols: &[String],
    tables: &[(String, usize, usize)],
    colref: &crate::parser::ColumnRef,
) -> Result<usize> {
    if let Some(t) = &colref.table {
        // Find the table segment first, then the column inside it.
        for (tname, width, offset) in tables {
            if tname.eq_ignore_ascii_case(t) {
                // Search qualified names within this segment.
                for i in 0..*width {
                    let idx = offset + i;
                    let stored = &combined_cols[idx];
                    let plain = stored.rsplit('.').next().unwrap_or(stored);
                    if plain.eq_ignore_ascii_case(&colref.col) {
                        return Ok(idx);
                    }
                }
                return Err(SqlError::InvalidColumnName(colref.dotted()));
            }
        }
        // Fall back to global qualified lookup (covers base table stored
        // plain when no joins preceded - not reachable here but harmless).
        resolve_col(combined_cols, &colref.dotted())
            .ok_or_else(|| SqlError::InvalidColumnName(colref.dotted()))
    } else {
        resolve_col(combined_cols, &colref.col)
            .ok_or_else(|| SqlError::InvalidColumnName(colref.dotted()))
    }
}

fn apply_where(
    rows: &[Vec<Value>],
    cols: &[String],
    filter: Option<&WhereClause>,
    bound: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let Some(f) = filter else {
        return Ok(rows.to_vec());
    };
    let map = column_index_map(cols);
    let compiled = f.compile(&map, bound)?;
    Ok(rows.iter().filter(|r| compiled.matches_row(r)).cloned().collect())
}

/// Resolve sub-select filters into static `WhereClause`s by executing each
/// sub-query once. Scalar `= (SELECT ...)` errors on multi-row output.
fn resolve_subselect_filter(
    conn: &Connection,
    filter: Option<WhereClause>,
    bound: &[Value],
) -> Result<Option<WhereClause>> {
    match filter {
        None => Ok(None),
        Some(f) => resolve_subselect_clause(conn, f, bound).map(Some),
    }
}

fn resolve_subselect_clause(
    conn: &Connection,
    clause: WhereClause,
    bound: &[Value],
) -> Result<WhereClause> {
    match clause {
        WhereClause::InSelect { col, query, negate } => {
            let rows = eval_select_to_rows(conn, &query, bound)?;
            let values: Vec<Expr> = rows
                .into_iter()
                .map(|r| {
                    r.values.first().cloned().unwrap_or(Value::Null)
                })
                .map(Expr::Literal)
                .collect();
            Ok(WhereClause::In { col, values, negate })
        }
        WhereClause::CmpSelect { col, op, query } => {
            let rows = eval_select_to_rows(conn, &query, bound)?;
            if rows.len() > 1 {
                return Err(SqlError::SqliteFailure {
                    code: 1,
                    message: "scalar sub-select returned more than one row".into(),
                });
            }
            match rows.into_iter().next() {
                None => {
                    // Empty scalar sub-select yields NULL, which never matches
                    // (`UNKNOWN` filters the row out). Encode as an always-false
                    // predicate: empty `OR` is false for every row.
                    if op == crate::parser::CmpOp::Eq {
                        Ok(WhereClause::Or(vec![]))
                    } else {
                        Ok(WhereClause::Cmp {
                            col,
                            op,
                            expr: Expr::Literal(Value::Null),
                        })
                    }
                }
                Some(row) => {
                    if row.values.len() != 1 {
                        return Err(SqlError::SqliteFailure {
                            code: 1,
                            message: "scalar sub-select must return exactly one column".into(),
                        });
                    }
                    let lit = Expr::Literal(row.values[0].clone());
                    if op == crate::parser::CmpOp::Eq {
                        Ok(WhereClause::Eq(col, lit))
                    } else {
                        Ok(WhereClause::Cmp { col, op, expr: lit })
                    }
                }
            }
        }
        WhereClause::And(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_subselect_clause(conn, item, bound)?);
            }
            Ok(WhereClause::And(out))
        }
        WhereClause::Or(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_subselect_clause(conn, item, bound)?);
            }
            Ok(WhereClause::Or(out))
        }
        other => Ok(other),
    }
}

fn project_single_table_row(
    table_cols: &[String],
    row: &[Value],
    items: &[SelectItem],
    star: bool,
) -> Result<Row> {
    if star {
        return Ok(Row {
            columns: Rc::new(table_cols.to_vec()),
            values: row.to_vec(),
        });
    }
    let map = column_index_map(table_cols);
    let mut out_cols = Vec::with_capacity(items.len());
    let mut out_vals = Vec::with_capacity(items.len());
    for item in items {
        if item.agg.is_some() {
            return Err(SqlError::unsupported("aggregates require grouped evaluation"));
        }
        let dotted = item
            .table
            .as_ref()
            .map(|t| format!("{t}.{}", item.col))
            .unwrap_or_else(|| item.col.clone());
        let idx = map
            .get(&dotted.to_ascii_lowercase())
            .copied()
            .or_else(|| {
                dotted
                    .rfind('.')
                    .and_then(|d| map.get(&dotted[d + 1..].to_ascii_lowercase()).copied())
            })
            .ok_or_else(|| SqlError::InvalidColumnName(dotted.clone()))?;
        out_cols.push(table_cols[idx].clone());
        out_vals.push(row[idx].clone());
    }
    Ok(Row { columns: Rc::new(out_cols), values: out_vals })
}

fn project_rows(
    combined_cols: &[String],
    rows: &[Vec<Value>],
    items: &[SelectItem],
    star: bool,
    base_table: &str,
    conn: &Connection,
) -> Result<Vec<Row>> {
    if star {
        // Single-table star keeps legacy plain names; joined star exposes
        // qualified names. The column list is identical for every row, so it
        // is shared by pointer across the whole result.
        let shared: SharedColumns = if combined_cols.iter().any(|c| c.contains('.')) {
            Rc::new(combined_cols.to_vec())
        } else {
            let inner = conn.inner.borrow();
            let tbl = inner.tables.get(&base_table.to_ascii_lowercase()).ok_or_else(|| {
                SqlError::SqliteFailure { code: 1, message: format!("no such table: {base_table}") }
            })?;
            Rc::new(tbl.columns.iter().map(|c| c.name.clone()).collect())
        };
        return Ok(rows
            .iter()
            .map(|r| Row { columns: shared.clone(), values: r.to_vec() })
            .collect());
    }
    let map = column_index_map(combined_cols);
    // Resolve the projection once: indices plus output names are identical for
    // every row, so the names are shared by pointer across the whole result.
    let mut proj: Vec<usize> = Vec::with_capacity(items.len());
    let mut out_names: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        debug_assert!(item.agg.is_none());
        let dotted = item
            .table
            .as_ref()
            .map(|t| format!("{t}.{}", item.col))
            .unwrap_or_else(|| item.col.clone());
        let idx = map
            .get(&dotted.to_ascii_lowercase())
            .copied()
            .or_else(|| {
                dotted
                    .rfind('.')
                    .and_then(|d| map.get(&dotted[d + 1..].to_ascii_lowercase()).copied())
            })
            .ok_or_else(|| SqlError::InvalidColumnName(dotted.clone()))?;
        // Output keeps the qualifier when the query used one, so joined
        // rows stay addressable by `table.col`.
        if item.table.is_some() {
            out_names.push(dotted);
        } else {
            // Map back to the stored plain name for legacy compatibility.
            let stored = &combined_cols[idx];
            out_names.push(stored.rsplit('.').next().unwrap_or(stored).to_owned());
        }
        proj.push(idx);
    }
    let shared: SharedColumns = Rc::new(out_names);
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut vals = Vec::with_capacity(proj.len());
        for &idx in &proj {
            vals.push(row[idx].clone());
        }
        out.push(Row { columns: shared.clone(), values: vals });
    }
    Ok(out)
}

fn eval_pure_agg(
    combined_cols: &[String],
    rows: &[Vec<Value>],
    items: &[SelectItem],
) -> Result<Row> {
    let map = column_index_map(combined_cols);
    let mut out_cols = Vec::with_capacity(items.len());
    let mut out_vals = Vec::with_capacity(items.len());
    for item in items {
        let Some(func) = item.agg else {
            return Err(SqlError::parse("non-aggregated column requires GROUP BY"));
        };
        out_cols.push(item.output_name());
        if item.col == "*" {
            out_vals.push(Value::Integer(rows.len() as i64));
        } else {
            let dotted = item
                .table
                .as_ref()
                .map(|t| format!("{t}.{}", item.col))
                .unwrap_or_else(|| item.col.clone());
            let idx = map
                .get(&dotted.to_ascii_lowercase())
                .copied()
                .or_else(|| {
                    dotted
                        .rfind('.')
                        .and_then(|d| map.get(&dotted[d + 1..].to_ascii_lowercase()).copied())
                })
                .ok_or_else(|| SqlError::InvalidColumnName(dotted))?;
            let vals: Vec<Value> = rows.iter().map(|r| r[idx].clone()).collect();
            out_vals.push(compute_agg(func, false, &vals));
        }
    }
    Ok(Row { columns: Rc::new(out_cols), values: out_vals })
}

fn eval_grouped(
    combined_cols: &[String],
    rows: &[Vec<Value>],
    items: &[SelectItem],
    group_by: &[String],
    having: Option<&HavingClause>,
    bound: &[Value],
) -> Result<Vec<Row>> {
    let map = column_index_map(combined_cols);
    let mut group_idxs = Vec::with_capacity(group_by.len());
    for g in group_by {
        let idx = map
            .get(&g.to_ascii_lowercase())
            .copied()
            .or_else(|| {
                g.rfind('.')
                    .and_then(|d| map.get(&g[d + 1..].to_ascii_lowercase()).copied())
            })
            .ok_or_else(|| SqlError::InvalidColumnName(g.clone()))?;
        group_idxs.push(idx);
    }
    // Group consecutive keys: linear search with `values_equal` keeps NULL
    // grouping (NULL == NULL) without hashing floats.
    let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    for (pos, row) in rows.iter().enumerate() {
        let key: Vec<Value> = group_idxs.iter().map(|&i| row[i].clone()).collect();
        let mut found = None;
        for (gi, (gkey, _)) in groups.iter().enumerate() {
            if gkey.len() == key.len()
                && gkey.iter().zip(key.iter()).all(|(a, b)| crate::parser::values_equal(a, b))
            {
                found = Some(gi);
                break;
            }
        }
        match found {
            Some(gi) => groups[gi].1.push(pos),
            None => groups.push((key, vec![pos])),
        }
    }
    // HAVING without GROUP BY over an empty input still yields one group.
    if groups.is_empty() && group_by.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    let mut out = Vec::new();
    // Output names depend only on the select list, not on the group, so they
    // are built once and shared by pointer across all group rows.
    let shared: SharedColumns = Rc::new(items.iter().map(|item| item.output_name()).collect());
    for (_, members) in &groups {
        let member_rows: Vec<&Vec<Value>> = members.iter().map(|&p| &rows[p]).collect();
        if let Some(h) = having {
            if !eval_having(h, combined_cols, &map, &member_rows, bound)? {
                continue;
            }
        }
        let mut vals = Vec::with_capacity(items.len());
        for item in items {
            match item.agg {
                Some(func) => {
                    if item.col == "*" {
                        vals.push(Value::Integer(member_rows.len() as i64));
                    } else {
                        let dotted = item
                            .table
                            .as_ref()
                            .map(|t| format!("{t}.{}", item.col))
                            .unwrap_or_else(|| item.col.clone());
                        let idx = map
                            .get(&dotted.to_ascii_lowercase())
                            .copied()
                            .or_else(|| {
                                dotted.rfind('.').and_then(|d| {
                                    map.get(&dotted[d + 1..].to_ascii_lowercase()).copied()
                                })
                            })
                            .ok_or_else(|| SqlError::InvalidColumnName(dotted))?;
                        let vs: Vec<Value> =
                            member_rows.iter().map(|r| r[idx].clone()).collect();
                        vals.push(compute_agg(func, false, &vs));
                    }
                }
                None => {
                    let dotted = item
                        .table
                        .as_ref()
                        .map(|t| format!("{t}.{}", item.col))
                        .unwrap_or_else(|| item.col.clone());
                    let idx = map
                        .get(&dotted.to_ascii_lowercase())
                        .copied()
                        .or_else(|| {
                            dotted.rfind('.').and_then(|d| {
                                map.get(&dotted[d + 1..].to_ascii_lowercase()).copied()
                            })
                        })
                        .ok_or_else(|| SqlError::InvalidColumnName(dotted))?;
                    // Grouped plain columns take the first row's value (equal
                    // to the group key when the column is grouped).
                    vals.push(member_rows.first().map(|r| r[idx].clone()).unwrap_or(Value::Null));
                }
            }
        }
        out.push(Row { columns: shared.clone(), values: vals });
    }
    Ok(out)
}

fn eval_having(
    clause: &HavingClause,
    combined_cols: &[String],
    map: &HashMap<String, usize>,
    member_rows: &[&Vec<Value>],
    bound: &[Value],
) -> Result<bool> {
    match clause {
        HavingClause::Cond(cond) => {
            let left_val = match &cond.left {
                HavingLeft::Column(name) => {
                    let idx = map
                        .get(&name.to_ascii_lowercase())
                        .copied()
                        .or_else(|| {
                            name.rfind('.')
                                .and_then(|d| map.get(&name[d + 1..].to_ascii_lowercase()).copied())
                        })
                        .ok_or_else(|| SqlError::InvalidColumnName(name.clone()))?;
                    member_rows.first().map(|r| r[idx].clone()).unwrap_or(Value::Null)
                }
                HavingLeft::Agg { func, table, col } => {
                    if col == "*" {
                        Value::Integer(member_rows.len() as i64)
                    } else {
                        let dotted = table
                            .as_ref()
                            .map(|t| format!("{t}.{col}"))
                            .unwrap_or_else(|| col.clone());
                        let idx = map
                            .get(&dotted.to_ascii_lowercase())
                            .copied()
                            .or_else(|| {
                                dotted.rfind('.').and_then(|d| {
                                    map.get(&dotted[d + 1..].to_ascii_lowercase()).copied()
                                })
                            })
                            .ok_or_else(|| SqlError::InvalidColumnName(dotted))?;
                        let vs: Vec<Value> =
                            member_rows.iter().map(|r| r[idx].clone()).collect();
                        compute_agg(*func, false, &vs)
                    }
                }
            };
            let right_val = match &cond.right {
                Expr::Literal(v) => v.clone(),
                Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
            };
            Ok(match cond.op {
                crate::parser::CmpOp::Eq => crate::parser::values_equal(&left_val, &right_val),
                op => crate::parser::eval_cmp(&left_val, &right_val, op),
            })
        }
        HavingClause::And(items) => {
            for item in items {
                if !eval_having(item, combined_cols, map, member_rows, bound)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        HavingClause::Or(items) => {
            for item in items {
                if eval_having(item, combined_cols, map, member_rows, bound)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

fn dedup_rows(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: HashSet<Vec<PkKey>> = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let key: Vec<PkKey> = row.values.iter().map(PkKey::of).collect();
        if seen.insert(key) {
            out.push(row);
        }
    }
    out
}

fn sort_rows_by_output(rows: &mut [Row], order_by: &[OrderBy]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let cols = rows[0].columns.clone();
    let mut keys = Vec::with_capacity(order_by.len());
    for key in order_by {
        let idx = resolve_col(&cols, &key.col)
            .ok_or_else(|| SqlError::InvalidColumnName(key.col.clone()))?;
        keys.push((idx, key.desc));
    }
    rows.sort_by(|a, b| {
        for (idx, desc) in &keys {
            let ord = sort_compare(&a.values[*idx], &b.values[*idx]);
            if ord != std::cmp::Ordering::Equal {
                return if *desc { ord.reverse() } else { ord };
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(())
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
/// never copied unfiltered. Grouped queries buffer only the filtered rows
/// of their query. `COUNT(*)` and other pure aggregates yield exactly one row.
pub struct MappedRows<'conn, F> {
    conn: &'conn Connection,
    stmt: Stmt,
    bound: Vec<Value>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// Compiled on first `next()` so `query_map` itself keeps its current
    /// behavior (table and column errors surface during iteration, exactly
    /// like the previous per-row resolution).
    plan: Option<ScanPlan>,
    pos: usize,
    skipped: usize,
    emitted: usize,
    /// Reused result-row buffer for the lazy single-table path: values are
    /// refilled per row instead of allocating a new `Row` (and its column
    /// list) per iteration. Safe because the mapping closure receives the
    /// row by reference and returns an owned value before the next refill.
    scratch: Row,
    sorted_init: bool,
    sorted: Vec<Row>,
    sorted_pos: usize,
    buffered_init: bool,
    buffered: Vec<Row>,
    buffered_pos: usize,
    join_init: bool,
    join_cols: Vec<String>,
    join_filter: Option<CompiledWhere>,
    join_proj: Vec<SelectItem>,
    join_star: bool,
    join_base_pos: usize,
    join_pending: Vec<Vec<Value>>,
    join_pending_pos: usize,
    subselect_resolved: bool,
    mapper: F,
}

impl<'conn, F> MappedRows<'conn, F> {
    fn new(
        conn: &'conn Connection,
        stmt: Stmt,
        bound: Vec<Value>,
        limit: Option<i64>,
        offset: Option<i64>,
        mapper: F,
    ) -> Self {
        Self {
            conn,
            stmt,
            bound,
            limit,
            offset,
            plan: None,
            pos: 0,
            skipped: 0,
            emitted: 0,
            scratch: Row { columns: Rc::new(Vec::new()), values: Vec::new() },
            sorted_init: false,
            sorted: Vec::new(),
            sorted_pos: 0,
            buffered_init: false,
            buffered: Vec::new(),
            buffered_pos: 0,
            join_init: false,
            join_cols: Vec::new(),
            join_filter: None,
            join_proj: Vec::new(),
            join_star: false,
            join_base_pos: 0,
            join_pending: Vec::new(),
            join_pending_pos: 0,
            subselect_resolved: false,
            mapper,
        }
    }

    fn needs_buffered(&self) -> bool {
        match &self.stmt {
            Stmt::Union { .. } => true,
            Stmt::Select { group_by, having, items, distinct, .. } => {
                !group_by.is_empty() || having.is_some() || items.iter().any(|i| i.agg.is_some()) || *distinct
            }
            _ => false,
        }
    }

    fn ensure_subselects(&mut self) -> Result<()> {
        if self.subselect_resolved {
            return Ok(());
        }
        self.subselect_resolved = true;
        // Only SELECT filters can contain sub-selects.
        let filter = match &self.stmt {
            Stmt::Select { filter, .. } => filter.clone(),
            _ => return Ok(()),
        };
        if !filter_has_subselect(filter.as_ref()) {
            return Ok(());
        }
        let resolved = resolve_subselect_filter(self.conn, filter, &self.bound)?;
        if let Stmt::Select { filter: f, .. } = &mut self.stmt {
            *f = resolved;
        }
        // Resolved IN with empty values stays as `In` with empty vec (allowed
        // here even though the parser rejects literal empty lists).
        Ok(())
    }

    /// Compile the scan plan on first use. Afterwards every row is evaluated
    /// with pure indexing: no table-key hashing beyond the lookup, no column
    /// clones, no string comparisons.
    fn ensure_plan(&mut self) -> Result<()> {
        if self.plan.is_none() {
            let (table, requested, star, filter) = match &self.stmt {
                Stmt::Select { table, columns, star, filter, .. } => {
                    (table.clone(), columns.clone(), *star, filter.clone())
                }
                _ => return Err(SqlError::ExecuteReturnedResults),
            };
            let plan = build_plan(self.conn, &table, &requested, star, filter.as_ref(), &self.bound)?;
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
        let (table_cols, positions, plan) = {
            let inner = self.conn.inner.borrow();
            // Clone the small plan for use after the borrow ends.
            let plan = self.plan.as_ref().expect("plan ensured").clone();
            let table_name = match &self.stmt {
                Stmt::Select { table, .. } => table.clone(),
                _ => String::new(),
            };
            let tbl = inner.tables.get(&plan.table_key).ok_or_else(|| {
                SqlError::SqliteFailure {
                    code: 1,
                    message: format!("no such table: {table_name}"),
                }
            })?;
            let table_cols: Vec<String> = tbl.columns.iter().map(|c| c.name.clone()).collect();
            // Buffer matching row positions only; full rows stay in the table
            // until the surviving window is projected below.
            let mut positions = Vec::new();
            for (pos, row) in tbl.rows.iter().enumerate() {
                let keep = match &plan.filter {
                    Some(f) => f.matches_row(row),
                    None => true,
                };
                if keep {
                    positions.push(pos);
                }
            }
            (table_cols, positions, plan)
        };
        let order_by = match &self.stmt {
            Stmt::Select { order_by, .. } => order_by.clone(),
            _ => Vec::new(),
        };
        let mut positions = positions;
        {
            let inner = self.conn.inner.borrow();
            let tbl = inner.tables.get(&plan.table_key).ok_or_else(|| SqlError::SqliteFailure {
                code: 1,
                message: "no such table".to_string(),
            })?;
            sort_row_positions(&tbl.rows, &table_cols, &mut positions, &order_by)?;
            let positions = apply_offset_limit(positions, self.offset, self.limit);
            let mut out = Vec::with_capacity(positions.len());
            for pos in &positions {
                out.push(project_planned(&plan, &tbl.rows[*pos]));
            }
            self.sorted = out;
        }
        self.sorted_pos = 0;
        self.sorted_init = true;
        Ok(())
    }

    fn init_buffered(&mut self) -> Result<()> {
        let rows = eval_select_to_rows(self.conn, &self.stmt, &self.bound)?;
        // `eval_select_to_rows` already applied per-side LIMIT/OFFSET for
        // UNION sides and single SELECTs. For UNION the outer iterator has no
        // extra window (outer limit is None); for single SELECTs the buffered
        // path (GROUP/DISTINCT/aggregates) already applied its window, so do
        // not trim twice. The only case needing outer trim is UNION with an
        // outer window, which never occurs (outer is None). Keep as-is.
        self.buffered = rows;
        self.buffered_pos = 0;
        self.buffered_init = true;
        Ok(())
    }

    fn init_join_lazy(&mut self) -> Result<()> {
        let (table, joins, filter, items, star) = match &self.stmt {
            Stmt::Select { table, joins, filter, items, star, .. } => {
                (table.clone(), joins.clone(), filter.clone(), items.clone(), *star)
            }
            _ => return Err(SqlError::ExecuteReturnedResults),
        };
        let combined = build_joined_rows(self.conn, &table, &joins)?;
        let map = column_index_map(&combined.cols);
        let resolved_filter = resolve_subselect_filter(self.conn, filter, &self.bound)?;
        let compiled = resolved_filter.as_ref().map(|f| f.compile(&map, &self.bound)).transpose()?;
        self.join_cols = combined.cols;
        self.join_filter = compiled;
        self.join_proj = items;
        self.join_star = star;
        self.join_base_pos = 0;
        self.join_pending = Vec::new();
        self.join_pending_pos = 0;
        self.join_init = true;
        // Update stored filter to resolved version for consistency.
        if let Stmt::Select { filter: f, .. } = &mut self.stmt {
            *f = resolved_filter;
        }
        Ok(())
    }

    fn next_join_row(&mut self) -> Result<Option<Row>> {
        // Borrow tables per base row; buffer only the fan-out of one base row.
        loop {
            if self.join_pending_pos < self.join_pending.len() {
                let combined = self.join_pending[self.join_pending_pos].clone();
                self.join_pending_pos += 1;
                if self.offset_remaining() > 0 {
                    self.skipped += 1;
                    continue;
                }
                if self.limit_reached() {
                    return Ok(None);
                }
                let row = project_joined_row(&self.join_cols, &combined, &self.join_proj, self.join_star)?;
                self.emitted += 1;
                return Ok(Some(row));
            }
            // Load next base fan-out.
            let (table, joins) = match &self.stmt {
                Stmt::Select { table, joins, .. } => (table.clone(), joins.clone()),
                _ => return Err(SqlError::ExecuteReturnedResults),
            };
            let next_batch: Option<Vec<Vec<Value>>> = {
                let inner = self.conn.inner.borrow();
                let base_tbl = inner.tables.get(&table.to_ascii_lowercase()).ok_or_else(|| {
                    SqlError::SqliteFailure { code: 1, message: format!("no such table: {table}") }
                })?;
                if self.join_base_pos >= base_tbl.rows.len() {
                    None
                } else {
                    let base_row = base_tbl.rows[self.join_base_pos].clone();
                    // Expand chained joins for this single base row.
                    let mut prefixes = vec![base_row];
                    for join in &joins {
                        let right_tbl = inner.tables.get(&join.table.to_ascii_lowercase()).ok_or_else(|| {
                            SqlError::SqliteFailure {
                                code: 1,
                                message: format!("no such table: {}", join.table),
                            }
                        })?;
                        // Resolve ON indices within current prefix width.
                        // Rebuild prefix column layout incrementally.
                        let mut next_prefixes = Vec::new();
                        for prefix in &prefixes {
                            // Left index: resolve against join_cols prefix slice.
                            // join_cols holds the full schema; prefix len tells
                            // current width.
                            let cur_width = prefix.len();
                            let prefix_cols = &self.join_cols[..cur_width.min(self.join_cols.len())];
                            let left_idx = resolve_join_side_lazy(prefix_cols, &join.left)?;
                            let right_cols: Vec<String> =
                                right_tbl.columns.iter().map(|c| c.name.clone()).collect();
                            let right_idx = resolve_col(&right_cols, &join.right.col)
                                .ok_or_else(|| SqlError::InvalidColumnName(join.right.col.clone()))?;
                            let left_val = &prefix[left_idx];
                            let mut matched_any = false;
                            for right_row in &right_tbl.rows {
                                if crate::parser::values_equal(left_val, &right_row[right_idx]) {
                                    matched_any = true;
                                    let mut combined_row = prefix.clone();
                                    combined_row.extend_from_slice(right_row);
                                    next_prefixes.push(combined_row);
                                }
                            }
                            if !matched_any && join.kind == JoinKind::Left {
                                let mut combined_row = prefix.clone();
                                combined_row.extend(
                                    std::iter::repeat(Value::Null).take(right_tbl.columns.len()),
                                );
                                next_prefixes.push(combined_row);
                            }
                        }
                        prefixes = next_prefixes;
                    }
                    Some(prefixes)
                }
            };
            match next_batch {
                None => return Ok(None),
                Some(prefixes) => {
                    self.join_base_pos += 1;
                    // Apply WHERE to this base row's fan-out only.
                    let mut passing = Vec::new();
                    for combined in prefixes {
                        let keep = match &self.join_filter {
                            Some(f) => f.matches_row(&combined),
                            None => true,
                        };
                        if keep {
                            passing.push(combined);
                        }
                    }
                    self.join_pending = passing;
                    self.join_pending_pos = 0;
                    // Loop to yield first passing row (or advance base).
                }
            }
        }
    }
}

impl<'conn, T, F> Iterator for MappedRows<'conn, F>
where
    F: FnMut(&Row) -> Result<T>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.subselect_resolved {
            if let Err(e) = self.ensure_subselects() {
                self.subselect_resolved = true;
                self.buffered_init = true;
                self.buffered.clear();
                self.buffered_pos = 0;
                // Return the error once; subsequent calls see empty buffer.
                // To avoid losing the error on retry, mark init done and
                // return error now.
                let _ = &self.buffered;
                return Some(Err(e));
            }
        }
        // Buffered modes: UNION, GROUP BY / HAVING / aggregates / DISTINCT.
        if self.needs_buffered() {
            if !self.buffered_init {
                match self.init_buffered() {
                    Ok(()) => {}
                    Err(e) => {
                        self.buffered_init = true;
                        return Some(Err(e));
                    }
                }
            }
            if self.buffered_pos >= self.buffered.len() {
                return None;
            }
            let row = self.buffered[self.buffered_pos].clone();
            self.buffered_pos += 1;
            return Some((self.mapper)(&row));
        }
        // JOIN paths.
        let has_joins = matches!(&self.stmt, Stmt::Select { joins, .. } if !joins.is_empty());
        if has_joins {
            let has_order = matches!(&self.stmt, Stmt::Select { order_by, .. } if !order_by.is_empty());
            if has_order {
                // ORDER over JOIN buffers only filtered joined rows.
                if !self.buffered_init {
                    match self.init_buffered() {
                        Ok(()) => {}
                        Err(e) => {
                            self.buffered_init = true;
                            return Some(Err(e));
                        }
                    }
                }
                if self.buffered_pos >= self.buffered.len() {
                    return None;
                }
                let row = self.buffered[self.buffered_pos].clone();
                self.buffered_pos += 1;
                return Some((self.mapper)(&row));
            }
            if !self.join_init {
                if let Err(e) = self.init_join_lazy() {
                    self.join_init = true;
                    return Some(Err(e));
                }
            }
            match self.next_join_row() {
                Ok(None) => None,
                Ok(Some(row)) => Some((self.mapper)(&row)),
                Err(e) => Some(Err(e)),
            }
        } else {
            // Single-table lazy paths (legacy behavior preserved).
            let has_order = matches!(&self.stmt, Stmt::Select { order_by, .. } if !order_by.is_empty());
            if has_order {
                if !self.sorted_init {
                    if let Err(e) = self.init_sorted() {
                        self.sorted_init = true;
                        return Some(Err(e));
                    }
                }
                if self.sorted_pos >= self.sorted.len() {
                    return None;
                }
                let row = self.sorted[self.sorted_pos].clone();
                self.sorted_pos += 1;
                return Some((self.mapper)(&row));
            }
            if self.limit_reached() {
                return None;
            }
            if let Err(e) = self.ensure_plan() {
                return Some(Err(e));
            }
            loop {
                // Fill the reused scratch row in place: no per-row `Row` or
                // column-list allocation. The borrow of `inner` ends before
                // the mapping closure runs, so the closure may still write
                // through the connection.
                {
                    let inner = self.conn.inner.borrow();
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
                    let table_name = match &self.stmt {
                        Stmt::Select { table, .. } => table.clone(),
                        _ => String::new(),
                    };
                    let tbl = match inner.tables.get(&plan.table_key) {
                        Some(tbl) => tbl,
                        None => {
                            return Some(Err(SqlError::SqliteFailure {
                                code: 1,
                                message: format!("no such table: {table_name}"),
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
                    // Disjoint field borrows: `plan` borrows `self.plan`
                    // while the scratch borrows `self.scratch`.
                    project_planned_into(plan, row, &mut self.scratch);
                };
                self.emitted += 1;
                return Some((self.mapper)(&self.scratch));
            }
        }
    }
}

fn project_joined_row(
    combined_cols: &[String],
    combined: &[Value],
    items: &[SelectItem],
    star: bool,
) -> Result<Row> {
    if star {
        return Ok(Row { columns: Rc::new(combined_cols.to_vec()), values: combined.to_vec() });
    }
    let map = column_index_map(combined_cols);
    let mut cols = Vec::with_capacity(items.len());
    let mut vals = Vec::with_capacity(items.len());
    for item in items {
        let dotted = item
            .table
            .as_ref()
            .map(|t| format!("{t}.{}", item.col))
            .unwrap_or_else(|| item.col.clone());
        let idx = map
            .get(&dotted.to_ascii_lowercase())
            .copied()
            .or_else(|| {
                dotted.rfind('.').and_then(|d| map.get(&dotted[d + 1..].to_ascii_lowercase()).copied())
            })
            .ok_or_else(|| SqlError::InvalidColumnName(dotted.clone()))?;
        if item.table.is_some() {
            cols.push(dotted);
        } else {
            let stored = &combined_cols[idx];
            cols.push(stored.rsplit('.').next().unwrap_or(stored).to_owned());
        }
        vals.push(combined[idx].clone());
    }
    Ok(Row { columns: Rc::new(cols), values: vals })
}

fn resolve_join_side_lazy(
    prefix_cols: &[String],
    colref: &crate::parser::ColumnRef,
) -> Result<usize> {
    if let Some(t) = &colref.table {
        for (idx, stored) in prefix_cols.iter().enumerate() {
            // Stored may be `table.col` (joined) or plain (base single).
            if let Some(dot) = stored.rfind('.') {
                let (st, sc) = (&stored[..dot], &stored[dot + 1..]);
                if st.eq_ignore_ascii_case(t) && sc.eq_ignore_ascii_case(&colref.col) {
                    return Ok(idx);
                }
            } else if stored.eq_ignore_ascii_case(&colref.col) {
                // Plain base column matches any qualifier on first match.
                return Ok(idx);
            }
        }
        return Err(SqlError::InvalidColumnName(colref.dotted()));
    }
    resolve_col(prefix_cols, &colref.col).ok_or_else(|| SqlError::InvalidColumnName(colref.dotted()))
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

    #[test]
    fn join_inner_and_left_with_null_padding() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE b (id INTEGER PRIMARY KEY, w TEXT);",
        )
        .unwrap();
        conn.execute("INSERT INTO a (id, v) VALUES (1, 'x'), (2, 'y')", params![]).unwrap();
        conn.execute("INSERT INTO b (id, w) VALUES (1, 'one')", params![]).unwrap();
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT a.id, b.w FROM a INNER JOIN b ON a.id = b.id")
            .unwrap()
            .query_map(params![], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, vec![(1, "one".to_string())]);
        let left: Vec<(i64, Option<String>)> = conn
            .prepare("SELECT a.id, b.w FROM a LEFT JOIN b ON a.id = b.id ORDER BY a.id ASC")
            .unwrap()
            .query_map(params![], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].1.as_deref(), Some("one"));
        assert_eq!(left[1].1, None);
    }

    #[test]
    fn subselect_in_and_scalar() {
        let conn = sample_conn();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM t WHERE age IN (SELECT age FROM t WHERE age = 20)")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(names.len(), 2);
        let name: String = conn
            .query_row("SELECT name FROM t WHERE age = (SELECT age FROM t WHERE id = 'a')", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "Alice");
        let multi = conn.query_row(
            "SELECT name FROM t WHERE age = (SELECT age FROM t)",
            params![],
            |row| row.get::<_, String>(0),
        );
        assert!(multi.is_err());
    }

    #[test]
    fn aggregates_without_group() {
        let conn = sample_conn();
        let sum: i64 = conn.query_row("SELECT SUM(age) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(sum, 95);
        let avg: f64 = conn.query_row("SELECT AVG(age) FROM t", params![], |row| row.get(0)).unwrap();
        assert!((avg - 23.75).abs() < 1e-9);
        let min: i64 = conn.query_row("SELECT MIN(age) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(min, 20);
        let max: i64 = conn.query_row("SELECT MAX(age) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(max, 30);
        let cnt: i64 = conn.query_row("SELECT COUNT(age) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(cnt, 4);
    }

    #[test]
    fn group_by_having_and_distinct_union_alter() {
        let conn = sample_conn();
        let rows: Vec<(i64, i64)> = conn
            .prepare("SELECT age, COUNT(*) FROM t GROUP BY age HAVING COUNT(*) > 1")
            .unwrap()
            .query_map(params![], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, vec![(20, 2)]);
        let distinct: Vec<i64> = conn
            .prepare("SELECT DISTINCT age FROM t ORDER BY age ASC")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(distinct, vec![20, 25, 30]);
        let union: Vec<i64> = conn
            .prepare("SELECT age FROM t WHERE age = 20 UNION SELECT age FROM t WHERE age = 30 ORDER BY age ASC")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(union, vec![20, 30]);
        let all: Vec<i64> = conn
            .prepare("SELECT age FROM t WHERE age = 20 UNION ALL SELECT age FROM t WHERE age = 20")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(all.len(), 4);
        conn.execute("ALTER TABLE t ADD COLUMN nick TEXT DEFAULT 'n/a'", params![]).unwrap();
        let nick: String = conn
            .query_row("SELECT nick FROM t WHERE id = 'a'", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(nick, "n/a");
    }

    #[test]
    fn shared_columns_scan_keeps_names_and_values() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER)")
            .unwrap();
        for i in 0..50i32 {
            conn.execute(
                "INSERT INTO t (id, name, v) VALUES (?1, ?2, ?3)",
                params![i, format!("n{i}"), i * 2],
            )
            .unwrap();
        }
        // Every streamed row carries the same shared schema; index- and
        // name-based getters must agree on all rows.
        let rows: Vec<(i64, String, i64)> = conn
            .prepare("SELECT id, name, v FROM t")
            .unwrap()
            .query_map(params![], |row| {
                assert_eq!(row.column_names(), &["id".to_string(), "name".to_string(), "v".to_string()]);
                let by_idx: String = row.get(1)?;
                let by_name: String = row.get("name")?;
                assert_eq!(by_idx, by_name);
                Ok((row.get::<_, i64>(0)?, by_idx, row.get::<_, i64>(2)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 50);
        for (i, (id, name, v)) in rows.iter().enumerate() {
            assert_eq!(*id, i as i64);
            assert_eq!(name, &format!("n{i}"));
            assert_eq!(*v, i as i64 * 2);
        }
        // Star projection shares the same names.
        let first: Vec<String> = conn
            .prepare("SELECT * FROM t ORDER BY id ASC LIMIT 1")
            .unwrap()
            .query_map(params![], |row| Ok(row.column_names().to_vec()))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(first, vec!["id".to_string(), "name".to_string(), "v".to_string()]);
    }

    #[test]
    fn scratch_reuse_keeps_every_row_distinct() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..200i32 {
            conn.execute(
                "INSERT INTO t (id, v) VALUES (?1, ?2)",
                params![i, format!("val-{i:03}")],
            )
            .unwrap();
        }
        // The lazy path refills one row buffer per iteration; mapping into
        // owned values must still yield every distinct row (no aliasing).
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT id, v FROM t")
            .unwrap()
            .query_map(params![], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 200);
        for (i, (id, v)) in rows.iter().enumerate() {
            assert_eq!(*id, i as i64);
            assert_eq!(v, &format!("val-{i:03}"));
        }
        // LIMIT / OFFSET over the reused buffer stay exact.
        let page: Vec<i64> = conn
            .prepare("SELECT id FROM t LIMIT 5 OFFSET 10")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(page, vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn order_window_matches_full_sort_with_ties() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)")
            .unwrap();
        // Groups with ties: insertion order within equal keys must be stable.
        let data = [(0i32, 1i32, 30i32), (1, 2, 10), (2, 1, 20), (3, 2, 40), (4, 1, 20), (5, 3, 50)];
        for (id, g, v) in data {
            conn.execute(
                "INSERT INTO t (id, g, v) VALUES (?1, ?2, ?3)",
                params![id, g, v],
            )
            .unwrap();
        }
        let asc: Vec<i64> = conn
            .prepare("SELECT id FROM t ORDER BY v ASC")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        // 10 -> id 1; 20 -> ids 2, 4 (insertion order); 30 -> id 0; 40 -> id 3; 50 -> id 5.
        assert_eq!(asc, vec![1, 2, 4, 0, 3, 5]);
        let desc_limit: Vec<i64> = conn
            .prepare("SELECT id FROM t ORDER BY v DESC LIMIT 3")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(desc_limit, vec![5, 3, 0]);
        // Multi-key ordering with offset.
        let multi: Vec<i64> = conn
            .prepare("SELECT id FROM t ORDER BY g ASC, v DESC LIMIT 10 OFFSET 2")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        // g=1: ids 0 (30), 2 (20), 4 (20); g=2: ids 3 (40), 1 (10); g=3: id 5.
        assert_eq!(multi, vec![4, 3, 1, 5]);
        // Filtered ORDER BY + LIMIT projects only the window.
        let filtered: Vec<i64> = conn
            .prepare("SELECT id FROM t WHERE g >= ?1 ORDER BY v DESC LIMIT 2")
            .unwrap()
            .query_map(params![2i32], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(filtered, vec![5, 3]);
        // Window past the end is empty; limit beyond the end returns all.
        let empty: Vec<i64> = conn
            .prepare("SELECT id FROM t ORDER BY v ASC LIMIT 5 OFFSET 100")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(empty.is_empty());
        let all: Vec<i64> = conn
            .prepare("SELECT id FROM t ORDER BY v ASC LIMIT 100")
            .unwrap()
            .query_map(params![], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(all.len(), 6);
    }

    #[test]
    fn count_star_fast_path_matches_general_path() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        // Empty table counts zero through every entry point.
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(n, 0);
        for i in 0..100i32 {
            conn.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params![i, i % 10]).unwrap();
        }
        // Filtered count via query_row, query_map (buffered) and query.
        for sql in [
            "SELECT COUNT(*) FROM t",
            "SELECT COUNT(*) FROM t WHERE v >= 5",
            "SELECT COUNT(*) FROM t WHERE v > 100",
        ] {
            let via_row: i64 = conn.query_row(sql, params![], |row| row.get(0)).unwrap();
            let via_map: i64 = conn
                .prepare(sql)
                .unwrap()
                .query_map(params![], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            let via_query = conn.prepare(sql).unwrap().query(params![]).unwrap();
            assert_eq!(via_query.len(), 1);
            let via_query_val: i64 = via_query[0].get(0).unwrap();
            assert_eq!(via_row, via_map);
            assert_eq!(via_row, via_query_val);
        }
        let full: i64 = conn.query_row("SELECT COUNT(*) FROM t", params![], |row| row.get(0)).unwrap();
        assert_eq!(full, 100);
        let half: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE v >= 5", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(half, 50);
        let none: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE v > 100", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(none, 0);
        // LIMIT is ignored for pure aggregates (legacy behavior preserved).
        let limited: i64 = conn
            .query_row("SELECT COUNT(*) FROM t LIMIT 3", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(limited, 100);
        // Sub-select filters still route through the general path correctly.
        let sub: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM t WHERE v IN (SELECT v FROM t WHERE v = 7)",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sub, 10);
    }
}
