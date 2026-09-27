//! Minimal SQL parser for the basis engine.
//!
//! Supports the statement subset CoreData and the basis tests need:
//! `PRAGMA`, `CREATE TABLE`, `CREATE INDEX`, `INSERT`, `SELECT`, `UPDATE`,
//! `DELETE`, plus `BEGIN` / `COMMIT` / `ROLLBACK` inside `execute_batch`.
//! The full SQLite grammar (JOIN, sub-selects, triggers, views, ...) is an
//! explicit roadmap item for follow-up subagents.

use crate::error::{Result, SqlError};
use crate::value::Value;
use std::borrow::Cow;
use std::collections::HashMap;

/// A literal or bound parameter inside a statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Placeholder(usize),
    Literal(Value),
}

/// Comparison operator inside a `Cmp` filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// Single `ORDER BY` key: column plus direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderBy {
    pub col: String,
    pub desc: bool,
}

/// `LIMIT` / `OFFSET` bound: either an integer literal or a bound parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitValue {
    Literal(i64),
    Placeholder(usize),
}

/// Aggregate function in the select list or `HAVING`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Avg => "AVG",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("COUNT") {
            Some(AggFunc::Count)
        } else if name.eq_ignore_ascii_case("SUM") {
            Some(AggFunc::Sum)
        } else if name.eq_ignore_ascii_case("AVG") {
            Some(AggFunc::Avg)
        } else if name.eq_ignore_ascii_case("MIN") {
            Some(AggFunc::Min)
        } else if name.eq_ignore_ascii_case("MAX") {
            Some(AggFunc::Max)
        } else {
            None
        }
    }
}

/// One item in the `SELECT` projection list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    pub table: Option<String>,
    pub col: String,
    pub agg: Option<AggFunc>,
}

impl SelectItem {
    pub fn output_name(&self) -> String {
        match self.agg {
            None => {
                if let Some(t) = &self.table {
                    format!("{t}.{}", self.col)
                } else {
                    self.col.clone()
                }
            }
            Some(func) => {
                if self.col == "*" {
                    format!("{}(*)", func.name())
                } else if let Some(t) = &self.table {
                    format!("{}({t}.{})", func.name(), self.col)
                } else {
                    format!("{}({})", func.name(), self.col)
                }
            }
        }
    }
}

/// Qualified column reference used by `JOIN ... ON`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnRef {
    pub table: Option<String>,
    pub col: String,
}

impl ColumnRef {
    pub fn dotted(&self) -> String {
        match &self.table {
            Some(t) => format!("{t}.{}", self.col),
            None => self.col.clone(),
        }
    }
}

/// Join kind for `FROM a [INNER | LEFT] JOIN b ON ...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

/// One `JOIN` clause with a column-equality `ON` condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: String,
    pub left: ColumnRef,
    pub right: ColumnRef,
}

/// Left side of a `HAVING` predicate: plain column or aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HavingLeft {
    Column(String),
    Agg {
        func: AggFunc,
        table: Option<String>,
        col: String,
    },
}

/// One `HAVING` comparison against a literal or bound parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct HavingCond {
    pub left: HavingLeft,
    pub op: CmpOp,
    pub right: Expr,
}

/// `HAVING` clause, chained with `AND` / `OR`.
#[derive(Clone, Debug, PartialEq)]
pub enum HavingClause {
    Cond(HavingCond),
    And(Vec<HavingClause>),
    Or(Vec<HavingClause>),
}

/// Filter condition, chained with AND / OR.
#[derive(Clone, Debug, PartialEq)]
pub enum WhereClause {
    Eq(String, Expr),
    Cmp {
        col: String,
        op: CmpOp,
        expr: Expr,
    },
    Like {
        col: String,
        pattern: Expr,
        negate: bool,
    },
    In {
        col: String,
        values: Vec<Expr>,
        negate: bool,
    },
    InSelect {
        col: String,
        query: Box<Stmt>,
        negate: bool,
    },
    CmpSelect {
        col: String,
        op: CmpOp,
        query: Box<Stmt>,
    },
    IsNull(String),
    IsNotNull(String),
    And(Vec<WhereClause>),
    Or(Vec<WhereClause>),
}

impl WhereClause {
    pub fn matches(&self, columns: &[String], row: &[Value], bound: &[Value]) -> Result<bool> {
        match self {
            WhereClause::Eq(col, expr) => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let expected = match expr {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                Ok(values_equal(&row[idx], &expected))
            }
            WhereClause::Cmp { col, op, expr } => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let expected = match expr {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                Ok(eval_cmp(&row[idx], &expected, *op))
            }
            WhereClause::Like { col, pattern, negate } => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let pat = match pattern {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                let matched = eval_like(&row[idx], &pat);
                Ok(if *negate { !matched } else { matched })
            }
            WhereClause::In { col, values, negate } => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let mut found = false;
                for expr in values {
                    let candidate = match expr {
                        Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                        Expr::Literal(v) => v.clone(),
                    };
                    if values_equal(&row[idx], &candidate) {
                        found = true;
                        break;
                    }
                }
                Ok(if *negate { !found } else { found })
            }
            WhereClause::IsNull(col) => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                Ok(row[idx].is_null())
            }
            WhereClause::IsNotNull(col) => {
                let idx = resolve_col(columns, col)
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                Ok(!row[idx].is_null())
            }
            WhereClause::InSelect { .. } | WhereClause::CmpSelect { .. } => Err(
                SqlError::unsupported("sub-selects require statement evaluation"),
            ),
            WhereClause::And(items) => {
                for item in items {
                    if !item.matches(columns, row, bound)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            WhereClause::Or(items) => {
                for item in items {
                    if item.matches(columns, row, bound)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}

/// Lowercase column-name to position map, built once per statement execution.
///
/// Uses first-match semantics, exactly like the `position()` scans in
/// [`WhereClause::matches`], so compiled evaluation agrees with it row for row.
/// Qualified names (`table.col`) also register their `col` suffix on first
/// match, and plain names match a trailing `.name` suffix, so `col` and
/// `table.col` resolve identically.
pub(crate) fn column_index_map(columns: &[String]) -> HashMap<String, usize> {
    let mut map = HashMap::with_capacity(columns.len() * 2);
    for (idx, name) in columns.iter().enumerate() {
        map.entry(name.to_ascii_lowercase()).or_insert(idx);
        if let Some(dot) = name.rfind('.') {
            map.entry(name[dot + 1..].to_ascii_lowercase()).or_insert(idx);
        }
    }
    // Plain lookup that also matches `table.col` suffixes (for `SELECT *`
    // style plain stores queried with qualified filters and vice versa).
    // Entries above already cover qualified stores; add reverse direction:
    // for plain stores, qualified filters fall back via `resolve_col`.
    map
}

/// Resolve a possibly qualified column name against `columns`.
///
/// Exact case-insensitive match wins; otherwise a `table.col` filter matches
/// a plain `col` store (suffix after the last `.`), and a plain filter
/// matches the first `*.col` store. Returns the row position.
pub(crate) fn resolve_col(columns: &[String], name: &str) -> Option<usize> {
    if let Some(pos) = columns.iter().position(|c| c.eq_ignore_ascii_case(name)) {
        return Some(pos);
    }
    if let Some(dot) = name.rfind('.') {
        let suffix = &name[dot + 1..];
        if let Some(pos) = columns.iter().position(|c| c.eq_ignore_ascii_case(suffix)) {
            return Some(pos);
        }
    } else {
        let qualified = format!(".{name}");
        if let Some(pos) = columns
            .iter()
            .position(|c| c.len() >= qualified.len() && c[..].to_ascii_lowercase().ends_with(&qualified.to_ascii_lowercase()))
        {
            return Some(pos);
        }
    }
    None
}

/// Resolve one expression against bound parameters a single time. Bounds are
/// fixed for the whole scan, so looking them up per row is pure overhead.
fn resolve_once(expr: &Expr, bound: &[Value]) -> Value {
    match expr {
        Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
        Expr::Literal(v) => v.clone(),
    }
}

/// A [`WhereClause`] with column names resolved to positions and bound
/// parameters resolved to values. Per-row evaluation then performs zero
/// string comparisons and zero parameter lookups.
#[derive(Clone, Debug)]
pub(crate) enum CompiledWhere {
    Eq(usize, Value),
    Cmp { idx: usize, op: CmpOp, expected: Value },
    Like { idx: usize, pattern: Value, negate: bool },
    In { idx: usize, values: Vec<Value>, negate: bool },
    IsNull(usize),
    IsNotNull(usize),
    And(Vec<CompiledWhere>),
    Or(Vec<CompiledWhere>),
}

impl WhereClause {
    /// Compile column names and bound parameters away. Fails with
    /// `InvalidColumnName` exactly like [`WhereClause::matches`].
    pub(crate) fn compile(
        &self,
        map: &HashMap<String, usize>,
        bound: &[Value],
    ) -> Result<CompiledWhere> {
        let idx = |col: &String| {
            if let Some(i) = map.get(&col.to_ascii_lowercase()).copied() {
                return Ok(i);
            }
            if let Some(dot) = col.rfind('.') {
                if let Some(i) = map.get(&col[dot + 1..].to_ascii_lowercase()).copied() {
                    return Ok(i);
                }
            }
            Err(SqlError::InvalidColumnName(col.clone()))
        };
        match self {
            WhereClause::Eq(col, expr) => Ok(CompiledWhere::Eq(idx(col)?, resolve_once(expr, bound))),
            WhereClause::Cmp { col, op, expr } => Ok(CompiledWhere::Cmp {
                idx: idx(col)?,
                op: *op,
                expected: resolve_once(expr, bound),
            }),
            WhereClause::Like { col, pattern, negate } => Ok(CompiledWhere::Like {
                idx: idx(col)?,
                pattern: resolve_once(pattern, bound),
                negate: *negate,
            }),
            WhereClause::In { col, values, negate } => Ok(CompiledWhere::In {
                idx: idx(col)?,
                values: values.iter().map(|e| resolve_once(e, bound)).collect(),
                negate: *negate,
            }),
            WhereClause::InSelect { .. } | WhereClause::CmpSelect { .. } => Err(
                SqlError::unsupported("sub-selects require statement evaluation"),
            ),
            WhereClause::IsNull(col) => Ok(CompiledWhere::IsNull(idx(col)?)),
            WhereClause::IsNotNull(col) => Ok(CompiledWhere::IsNotNull(idx(col)?)),
            WhereClause::And(items) => items
                .iter()
                .map(|item| item.compile(map, bound))
                .collect::<Result<Vec<_>>>()
                .map(CompiledWhere::And),
            WhereClause::Or(items) => items
                .iter()
                .map(|item| item.compile(map, bound))
                .collect::<Result<Vec<_>>>()
                .map(CompiledWhere::Or),
        }
    }
}

impl CompiledWhere {
    /// Evaluate one row. Infallible: unknown columns were already rejected by
    /// [`WhereClause::compile`], and `NULL` / type-mismatch rules mirror
    /// [`WhereClause::matches`] exactly.
    pub(crate) fn matches_row(&self, row: &[Value]) -> bool {
        match self {
            CompiledWhere::Eq(idx, expected) => values_equal(&row[*idx], expected),
            CompiledWhere::Cmp { idx, op, expected } => eval_cmp(&row[*idx], expected, *op),
            CompiledWhere::Like { idx, pattern, negate } => {
                let matched = eval_like_borrowed(&row[*idx], pattern);
                if *negate { !matched } else { matched }
            }
            CompiledWhere::In { idx, values, negate } => {
                let mut found = false;
                for candidate in values {
                    if values_equal(&row[*idx], candidate) {
                        found = true;
                        break;
                    }
                }
                if *negate { !found } else { found }
            }
            CompiledWhere::IsNull(idx) => row[*idx].is_null(),
            CompiledWhere::IsNotNull(idx) => !row[*idx].is_null(),
            CompiledWhere::And(items) => items.iter().all(|item| item.matches_row(row)),
            CompiledWhere::Or(items) => items.iter().any(|item| item.matches_row(row)),
        }
    }
}

pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        (Value::Real(x), Value::Real(y)) => (x - y).abs() < f64::EPSILON,
        (Value::Integer(x), Value::Real(y)) => (*x as f64 - *y).abs() < f64::EPSILON,
        (Value::Real(x), Value::Integer(y)) => (*x - *y as f64).abs() < f64::EPSILON,
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Blob(x), Value::Blob(y)) => x == y,
        (Value::Text(x), Value::Integer(y)) => x == &y.to_string(),
        (Value::Integer(x), Value::Text(y)) => &x.to_string() == y,
        _ => false,
    }
}

/// Evaluate one comparison between a stored value and the expected value.
///
/// `NULL` on either side never matches (SQL `UNKNOWN` filters the row out),
/// including `!=` / `<>`: use `IS NULL` / `IS NOT NULL` to test for nulls.
pub(crate) fn eval_cmp(actual: &Value, expected: &Value, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => values_equal(actual, expected),
        CmpOp::NotEq => {
            if actual.is_null() || expected.is_null() {
                return false;
            }
            !values_equal(actual, expected)
        }
        CmpOp::Lt | CmpOp::LtEq | CmpOp::Gt | CmpOp::GtEq => {
            let ord = match compare_values(actual, expected) {
                Some(ord) => ord,
                None => return false,
            };
            match op {
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::LtEq => ord != std::cmp::Ordering::Greater,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                CmpOp::GtEq => ord != std::cmp::Ordering::Less,
                _ => false,
            }
        }
    }
}

/// Ordered comparison used by `<`, `<=`, `>`, `>=`.
///
/// Returns `None` for `NULL` operands and for mismatched type pairs, so the
/// row is filtered out instead of guessing an ordering.
pub(crate) fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(x), Value::Integer(y)) => Some(x.cmp(y)),
        (Value::Integer(x), Value::Real(y)) => (*x as f64).partial_cmp(y),
        (Value::Real(x), Value::Integer(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Total ordering used by `ORDER BY`: `NULL` first, then numbers, then text,
/// then blobs. Unlike [`compare_values`], mismatched types never fail; they
/// sort by type rank so the output order stays deterministic.
pub(crate) fn sort_compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Integer(x), Value::Real(y)) => {
            (*x as f64).partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::Real(x), Value::Integer(y)) => x
            .partial_cmp(&(*y as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

/// Borrow a value as text without cloning heap data: `Text` and `Blob`
/// (when valid UTF-8) borrow in place; numbers format once into an owned
/// string. Used by `LIKE` so full-table scans over text never clone per row.
fn value_as_text<'a>(value: &'a Value) -> Option<Cow<'a, str>> {
    match value {
        Value::Text(s) => Some(Cow::Borrowed(s)),
        Value::Integer(v) => Some(Cow::Owned(v.to_string())),
        Value::Real(v) => Some(Cow::Owned(v.to_string())),
        Value::Blob(bytes) => std::str::from_utf8(bytes).ok().map(Cow::Borrowed),
        Value::Null => None,
    }
}

pub(crate) fn eval_like(actual: &Value, pattern: &Value) -> bool {
    eval_like_borrowed(actual, pattern)
}

/// Allocation-free `LIKE` evaluation on borrowed text.
pub(crate) fn eval_like_borrowed(actual: &Value, pattern: &Value) -> bool {
    match (value_as_text(actual), value_as_text(pattern)) {
        (Some(text), Some(pat)) => like_match(&text, &pat),
        _ => false,
    }
}

/// Case-sensitive `LIKE` matcher: `%` spans any run (possibly empty) and `_`
/// matches exactly one character.
pub(crate) fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '_' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == '%' {
            star = Some(pi);
            retry = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '%' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Compute one aggregate over already-filtered values.
///
/// `COUNT(*)` counts every row (pass all values including `NULL`); the other
/// functions skip `NULL`. Empty input yields `0` for `COUNT` and `NULL`
/// otherwise. `SUM` returns `Integer` when all inputs are integers and
/// `Real` otherwise; `AVG` always returns `Real`.
pub(crate) fn compute_agg(func: AggFunc, is_star: bool, values: &[Value]) -> Value {
    match func {
        AggFunc::Count if is_star => Value::Integer(values.len() as i64),
        AggFunc::Count => {
            let n = values.iter().filter(|v| !v.is_null()).count();
            Value::Integer(n as i64)
        }
        AggFunc::Sum => {
            let mut ints: i64 = 0;
            let mut real_sum: f64 = 0.0;
            let mut seen_real = false;
            let mut seen_any = false;
            for v in values {
                match v {
                    Value::Null => {}
                    Value::Integer(i) => {
                        ints = ints.wrapping_add(*i);
                        real_sum += *i as f64;
                        seen_any = true;
                    }
                    Value::Real(f) => {
                        real_sum += *f;
                        seen_real = true;
                        seen_any = true;
                    }
                    _ => {}
                }
            }
            if !seen_any {
                Value::Null
            } else if seen_real {
                Value::Real(real_sum)
            } else {
                Value::Integer(ints)
            }
        }
        AggFunc::Avg => {
            let mut sum = 0.0;
            let mut n = 0usize;
            for v in values {
                match v {
                    Value::Null => {}
                    Value::Integer(i) => {
                        sum += *i as f64;
                        n += 1;
                    }
                    Value::Real(f) => {
                        sum += *f;
                        n += 1;
                    }
                    _ => {}
                }
            }
            if n == 0 {
                Value::Null
            } else {
                Value::Real(sum / n as f64)
            }
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in values {
                if v.is_null() {
                    continue;
                }
                match best {
                    None => best = Some(v),
                    Some(b) => {
                        let ord = sort_compare(v, b);
                        let better = match func {
                            AggFunc::Min => ord == std::cmp::Ordering::Less,
                            _ => ord == std::cmp::Ordering::Greater,
                        };
                        if better {
                            best = Some(v);
                        }
                    }
                }
            }
            best.cloned().unwrap_or(Value::Null)
        }
    }
}

/// Column definition inside `CREATE TABLE` / `ALTER TABLE ... ADD COLUMN`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub coltype: String,
    pub primary_key: bool,
    pub not_null: bool,
    pub default: Option<Value>,
}

/// A single parsed statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Pragma {
        name: String,
        value: Option<String>,
    },
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
        if_not_exists: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        if_not_exists: bool,
    },
    Insert {
        or_replace: bool,
        or_ignore: bool,
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Expr>>,
    },
    Select {
        distinct: bool,
        items: Vec<SelectItem>,
        columns: Vec<String>,
        star: bool,
        table: String,
        joins: Vec<JoinClause>,
        filter: Option<WhereClause>,
        group_by: Vec<String>,
        having: Option<HavingClause>,
        order_by: Vec<OrderBy>,
        limit: Option<LimitValue>,
        offset: Option<LimitValue>,
        count_star: bool,
    },
    Union {
        left: Box<Stmt>,
        right: Box<Stmt>,
        all: bool,
    },
    AlterTable {
        table: String,
        column: ColumnDef,
    },
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<WhereClause>,
    },
    Delete {
        table: String,
        filter: Option<WhereClause>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    Integer(i64),
    Real(f64),
    Text(String),
    Placeholder(usize),
    Named(String),
    Question,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Comma,
    Semi,
    LParen,
    RParen,
    Star,
    Dot,
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        match c {
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            ';' => {
                tokens.push(Token::Semi);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Eq);
                i += 1;
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::NotEq);
                    i += 2;
                } else {
                    return Err(SqlError::parse("expected '=' after '!'"));
                }
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::LessEq);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '>' {
                    tokens.push(Token::NotEq);
                    i += 2;
                } else {
                    tokens.push(Token::Less);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::GreaterEq);
                    i += 2;
                } else {
                    tokens.push(Token::Greater);
                    i += 1;
                }
            }
            ':' | '@' => {
                let marker = c;
                i += 1;
                let mut s = String::new();
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    s.push(chars[i]);
                    i += 1;
                }
                if s.is_empty() {
                    return Err(SqlError::parse(format!("expected parameter name after '{marker}'")));
                }
                tokens.push(Token::Named(s));
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                tokens.push(Token::Text(s));
            }
            '"' | '`' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    s.push(chars[i]);
                    i += 1;
                }
                i += 1;
                tokens.push(Token::Word(s));
            }
            '?' => {
                i += 1;
                let mut num = String::new();
                while i < chars.len() && chars[i].is_ascii_digit() {
                    num.push(chars[i]);
                    i += 1;
                }
                if num.is_empty() {
                    tokens.push(Token::Question);
                } else {
                    let n: usize = num
                        .parse()
                        .map_err(|_| SqlError::parse("invalid placeholder number"))?;
                    tokens.push(Token::Placeholder(n));
                }
            }
            _ if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
                || (c == '+' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let mut s = String::new();
                s.push(c);
                i += 1;
                let mut is_real = false;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E' || chars[i] == '+' || chars[i] == '-') {
                    if chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E' {
                        is_real = true;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if is_real {
                    let v: f64 = s.parse().map_err(|_| SqlError::parse(format!("invalid number: {s}")))?;
                    tokens.push(Token::Real(v));
                } else {
                    let v: i64 = s.parse().map_err(|_| SqlError::parse(format!("invalid number: {s}")))?;
                    tokens.push(Token::Integer(v));
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    s.push(chars[i]);
                    i += 1;
                }
                tokens.push(Token::Word(s));
            }
            _ => return Err(SqlError::parse(format!("unexpected character: {c}"))),
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect_word(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Word(w)) => Ok(w),
            other => Err(SqlError::parse(format!("expected identifier, got {other:?}"))),
        }
    }

    fn eat_word(&mut self, name: &str) -> bool {
        match self.peek() {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn parse_expr(&mut self, placeholder_counter: &mut usize) -> Result<Expr> {
        match self.next() {
            Some(Token::Question) => {
                *placeholder_counter += 1;
                Ok(Expr::Placeholder(*placeholder_counter))
            }
            Some(Token::Placeholder(n)) => {
                *placeholder_counter = (*placeholder_counter).max(n);
                Ok(Expr::Placeholder(n))
            }
            // Named parameters (`:name`, `@name`) bind by position in order
            // of appearance, exactly like `?`.
            Some(Token::Named(_)) => {
                *placeholder_counter += 1;
                Ok(Expr::Placeholder(*placeholder_counter))
            }
            Some(Token::Integer(v)) => Ok(Expr::Literal(Value::Integer(v))),
            Some(Token::Real(v)) => Ok(Expr::Literal(Value::Real(v))),
            Some(Token::Text(s)) => Ok(Expr::Literal(Value::Text(s))),
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("NULL") => Ok(Expr::Literal(Value::Null)),
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("TRUE") => {
                Ok(Expr::Literal(Value::Integer(1)))
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("FALSE") => {
                Ok(Expr::Literal(Value::Integer(0)))
            }
            other => Err(SqlError::parse(format!("expected expression, got {other:?}"))),
        }
    }

    fn parse_where(&mut self, placeholder_counter: &mut usize) -> Result<WhereClause> {
        self.parse_or(placeholder_counter)
    }

    /// `OR` binds looser than `AND`: `a OR b AND c` parses as `a OR (b AND c)`.
    fn parse_or(&mut self, placeholder_counter: &mut usize) -> Result<WhereClause> {
        let mut items = vec![self.parse_and(placeholder_counter)?];
        while self.eat_word("OR") {
            items.push(self.parse_and(placeholder_counter)?);
        }
        if items.len() == 1 {
            Ok(items.into_iter().next().unwrap())
        } else {
            Ok(WhereClause::Or(items))
        }
    }

    fn parse_and(&mut self, placeholder_counter: &mut usize) -> Result<WhereClause> {
        let mut items = vec![self.parse_condition(placeholder_counter)?];
        while self.eat_word("AND") {
            items.push(self.parse_condition(placeholder_counter)?);
        }
        if items.len() == 1 {
            Ok(items.into_iter().next().unwrap())
        } else {
            Ok(WhereClause::And(items))
        }
    }

    fn parse_column_ref(&mut self) -> Result<ColumnRef> {
        let first = self.expect_word()?;
        if matches!(self.peek(), Some(Token::Dot)) {
            self.pos += 1;
            let second = self.expect_word()?;
            Ok(ColumnRef {
                table: Some(first),
                col: second,
            })
        } else {
            Ok(ColumnRef {
                table: None,
                col: first,
            })
        }
    }

    fn parse_dotted_name(&mut self) -> Result<String> {
        Ok(self.parse_column_ref()?.dotted())
    }

    fn is_subselect_start(&self) -> bool {
        matches!(self.peek(), Some(Token::LParen))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("SELECT"))
    }

    fn parse_subselect(&mut self, placeholder_counter: &mut usize) -> Result<Box<Stmt>> {
        match self.next() {
            Some(Token::LParen) => {}
            other => return Err(SqlError::parse(format!("expected '(' before sub-select, got {other:?}"))),
        }
        let select_word = self.expect_word()?;
        if !select_word.eq_ignore_ascii_case("SELECT") {
            return Err(SqlError::parse(format!("expected SELECT in sub-select, got {select_word}")));
        }
        let stmt = self.parse_select_body(&select_word, placeholder_counter)?;
        // Sub-selects may themselves be UNIONs.
        let stmt = self.finish_union(stmt, placeholder_counter)?;
        match self.next() {
            Some(Token::RParen) => {}
            other => return Err(SqlError::parse(format!("expected ')' after sub-select, got {other:?}"))),
        }
        Ok(Box::new(stmt))
    }

    fn parse_condition(&mut self, placeholder_counter: &mut usize) -> Result<WhereClause> {
        let col = self.parse_dotted_name()?;
        if self.eat_word("IS") {
            let not = self.eat_word("NOT");
            match self.next() {
                Some(Token::Word(w)) if w.eq_ignore_ascii_case("NULL") => {}
                other => {
                    return Err(SqlError::parse(format!("expected NULL after IS, got {other:?}")))
                }
            }
            if not {
                return Ok(WhereClause::IsNotNull(col));
            }
            return Ok(WhereClause::IsNull(col));
        }
        if self.eat_word("NOT") {
            if self.eat_word("LIKE") {
                let pattern = self.parse_expr(placeholder_counter)?;
                return Ok(WhereClause::Like { col, pattern, negate: true });
            }
            if self.eat_word("IN") {
                if self.is_subselect_start() {
                    let query = self.parse_subselect(placeholder_counter)?;
                    return Ok(WhereClause::InSelect { col, query, negate: true });
                }
                let values = self.parse_in_list(placeholder_counter)?;
                return Ok(WhereClause::In { col, values, negate: true });
            }
            return Err(SqlError::parse("expected LIKE or IN after NOT"));
        }
        if self.eat_word("LIKE") {
            let pattern = self.parse_expr(placeholder_counter)?;
            return Ok(WhereClause::Like { col, pattern, negate: false });
        }
        if self.eat_word("IN") {
            if self.is_subselect_start() {
                let query = self.parse_subselect(placeholder_counter)?;
                return Ok(WhereClause::InSelect { col, query, negate: false });
            }
            let values = self.parse_in_list(placeholder_counter)?;
            return Ok(WhereClause::In { col, values, negate: false });
        }
        let op = match self.next() {
            Some(Token::Eq) => CmpOp::Eq,
            Some(Token::NotEq) => CmpOp::NotEq,
            Some(Token::Less) => CmpOp::Lt,
            Some(Token::LessEq) => CmpOp::LtEq,
            Some(Token::Greater) => CmpOp::Gt,
            Some(Token::GreaterEq) => CmpOp::GtEq,
            other => {
                return Err(SqlError::parse(format!("expected operator, got {other:?}")))
            }
        };
        if self.is_subselect_start() {
            let query = self.parse_subselect(placeholder_counter)?;
            if op == CmpOp::Eq {
                // Scalar `= (SELECT ...)` expects a single row; multi-row is an
                // execution error. Keep `Eq` shape via `CmpSelect` with `Eq`.
                return Ok(WhereClause::CmpSelect { col, op, query });
            }
            return Ok(WhereClause::CmpSelect { col, op, query });
        }
        let expr = self.parse_expr(placeholder_counter)?;
        match op {
            CmpOp::Eq => Ok(WhereClause::Eq(col, expr)),
            op => Ok(WhereClause::Cmp { col, op, expr }),
        }
    }

    fn parse_in_list(&mut self, placeholder_counter: &mut usize) -> Result<Vec<Expr>> {
        match self.next() {
            Some(Token::LParen) => {}
            other => return Err(SqlError::parse(format!("expected '(' after IN, got {other:?}"))),
        }
        if matches!(self.peek(), Some(Token::RParen)) {
            return Err(SqlError::parse("IN list must not be empty"));
        }
        let mut values = Vec::new();
        loop {
            values.push(self.parse_expr(placeholder_counter)?);
            match self.next() {
                Some(Token::Comma) => continue,
                Some(Token::RParen) => break,
                other => {
                    return Err(SqlError::parse(format!("expected ',' or ')', got {other:?}")))
                }
            }
        }
        Ok(values)
    }

    fn parse_order_by(&mut self) -> Result<Vec<OrderBy>> {
        if !self.eat_word("ORDER") {
            return Ok(Vec::new());
        }
        if !self.eat_word("BY") {
            return Err(SqlError::parse("expected BY after ORDER"));
        }
        let mut order_by = Vec::new();
        loop {
            let col = self.parse_dotted_name()?;
            let mut desc = false;
            if self.eat_word("DESC") {
                desc = true;
            } else {
                self.eat_word("ASC");
            }
            order_by.push(OrderBy { col, desc });
            if matches!(self.peek(), Some(Token::Comma)) {
                self.pos += 1;
                continue;
            }
            break;
        }
        Ok(order_by)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        // Aggregate form `FUNC(col)` / `FUNC(t.col)` / `COUNT(*)`.
        if let Some(Token::Word(w)) = self.peek().cloned() {
            if AggFunc::parse(&w).is_some()
                && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                let func = AggFunc::parse(&w).expect("checked above");
                self.pos += 1; // FUNC
                self.pos += 1; // (
                if func == AggFunc::Count && matches!(self.peek(), Some(Token::Star)) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::RParen) => {}
                        other => {
                            return Err(SqlError::parse(format!("expected ')', got {other:?}")))
                        }
                    }
                    return Ok(SelectItem {
                        table: None,
                        col: "*".to_owned(),
                        agg: Some(func),
                    });
                }
                let colref = self.parse_column_ref()?;
                if colref.col == "*" {
                    return Err(SqlError::parse("only COUNT(*) may use '*'"));
                }
                match self.next() {
                    Some(Token::RParen) => {}
                    other => return Err(SqlError::parse(format!("expected ')', got {other:?}"))),
                }
                return Ok(SelectItem {
                    table: colref.table,
                    col: colref.col,
                    agg: Some(func),
                });
            }
        }
        let colref = self.parse_column_ref()?;
        Ok(SelectItem {
            table: colref.table,
            col: colref.col,
            agg: None,
        })
    }

    fn parse_joins(&mut self) -> Result<Vec<JoinClause>> {
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_word("INNER") {
                if !self.eat_word("JOIN") {
                    return Err(SqlError::parse("expected JOIN after INNER"));
                }
                JoinKind::Inner
            } else if self.eat_word("LEFT") {
                self.eat_word("OUTER");
                if !self.eat_word("JOIN") {
                    return Err(SqlError::parse("expected JOIN after LEFT"));
                }
                JoinKind::Left
            } else if matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("JOIN")) {
                self.pos += 1;
                JoinKind::Inner
            } else {
                break;
            };
            let table = self.expect_word()?;
            if !self.eat_word("ON") {
                return Err(SqlError::parse("expected ON in JOIN"));
            }
            let left = self.parse_column_ref()?;
            match self.next() {
                Some(Token::Eq) => {}
                other => {
                    return Err(SqlError::parse(format!(
                        "expected '=' in JOIN ON condition, got {other:?}"
                    )))
                }
            }
            let right = self.parse_column_ref()?;
            joins.push(JoinClause { kind, table, left, right });
        }
        Ok(joins)
    }

    fn parse_group_by(&mut self) -> Result<Vec<String>> {
        if !matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("GROUP")) {
            return Ok(Vec::new());
        }
        self.pos += 1;
        if !self.eat_word("BY") {
            return Err(SqlError::parse("expected BY after GROUP"));
        }
        let mut cols = Vec::new();
        loop {
            cols.push(self.parse_dotted_name()?);
            if matches!(self.peek(), Some(Token::Comma)) {
                self.pos += 1;
                continue;
            }
            break;
        }
        if cols.is_empty() {
            return Err(SqlError::parse("GROUP BY requires at least one column"));
        }
        Ok(cols)
    }

    fn parse_having(&mut self, placeholder_counter: &mut usize) -> Result<Option<HavingClause>> {
        if !self.eat_word("HAVING") {
            return Ok(None);
        }
        Ok(Some(self.parse_having_or(placeholder_counter)?))
    }

    fn parse_having_or(&mut self, placeholder_counter: &mut usize) -> Result<HavingClause> {
        let mut items = vec![self.parse_having_and(placeholder_counter)?];
        while self.eat_word("OR") {
            items.push(self.parse_having_and(placeholder_counter)?);
        }
        if items.len() == 1 {
            Ok(items.into_iter().next().unwrap())
        } else {
            Ok(HavingClause::Or(items))
        }
    }

    fn parse_having_and(&mut self, placeholder_counter: &mut usize) -> Result<HavingClause> {
        let mut items = vec![self.parse_having_cond(placeholder_counter)?];
        while self.eat_word("AND") {
            items.push(self.parse_having_cond(placeholder_counter)?);
        }
        if items.len() == 1 {
            Ok(items.into_iter().next().unwrap())
        } else {
            Ok(HavingClause::And(items))
        }
    }

    fn parse_having_cond(&mut self, placeholder_counter: &mut usize) -> Result<HavingClause> {
        // Aggregate left side `FUNC(col) OP expr` or plain `[t.]col OP expr`.
        let left = if let Some(Token::Word(w)) = self.peek().cloned() {
            if AggFunc::parse(&w).is_some()
                && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                let func = AggFunc::parse(&w).expect("checked above");
                self.pos += 1;
                self.pos += 1;
                if func == AggFunc::Count && matches!(self.peek(), Some(Token::Star)) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::RParen) => {}
                        other => {
                            return Err(SqlError::parse(format!("expected ')', got {other:?}")))
                        }
                    }
                    HavingLeft::Agg { func, table: None, col: "*".to_owned() }
                } else {
                    let colref = self.parse_column_ref()?;
                    match self.next() {
                        Some(Token::RParen) => {}
                        other => {
                            return Err(SqlError::parse(format!("expected ')', got {other:?}")))
                        }
                    }
                    HavingLeft::Agg { func, table: colref.table, col: colref.col }
                }
            } else {
                HavingLeft::Column(self.parse_dotted_name()?)
            }
        } else {
            return Err(SqlError::parse("expected HAVING condition"));
        };
        let op = match self.next() {
            Some(Token::Eq) => CmpOp::Eq,
            Some(Token::NotEq) => CmpOp::NotEq,
            Some(Token::Less) => CmpOp::Lt,
            Some(Token::LessEq) => CmpOp::LtEq,
            Some(Token::Greater) => CmpOp::Gt,
            Some(Token::GreaterEq) => CmpOp::GtEq,
            other => return Err(SqlError::parse(format!("expected operator in HAVING, got {other:?}"))),
        };
        let right = self.parse_expr(placeholder_counter)?;
        Ok(HavingClause::Cond(HavingCond { left, op, right }))
    }

    fn parse_default_literal(&mut self) -> Result<Value> {
        match self.next() {
            Some(Token::Integer(v)) => Ok(Value::Integer(v)),
            Some(Token::Real(v)) => Ok(Value::Real(v)),
            Some(Token::Text(s)) => Ok(Value::Text(s)),
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("TRUE") => Ok(Value::Integer(1)),
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("FALSE") => Ok(Value::Integer(0)),
            other => Err(SqlError::parse(format!("expected DEFAULT literal, got {other:?}"))),
        }
    }

    fn parse_select_body(
        &mut self,
        _select_word: &str,
        placeholder_counter: &mut usize,
    ) -> Result<Stmt> {
        let mut distinct = false;
        if self.eat_word("DISTINCT") {
            distinct = true;
        } else {
            self.eat_word("ALL");
        }
        let mut star = false;
        let mut items: Vec<SelectItem> = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        if matches!(self.peek(), Some(Token::Star)) {
            self.pos += 1;
            star = true;
            if distinct {
                // `SELECT DISTINCT *` is valid; keep both flags.
            }
        } else {
            loop {
                let item = self.parse_select_item()?;
                columns.push(item.output_name());
                items.push(item);
                if matches!(self.peek(), Some(Token::Comma)) {
                    self.pos += 1;
                    continue;
                }
                break;
            }
            if items.is_empty() {
                return Err(SqlError::parse("SELECT requires at least one column"));
            }
        }
        if !self.eat_word("FROM") {
            return Err(SqlError::parse("expected FROM in SELECT"));
        }
        let table = self.expect_word()?;
        let joins = self.parse_joins()?;
        let filter = if self.eat_word("WHERE") {
            Some(self.parse_where(placeholder_counter)?)
        } else {
            None
        };
        let group_by = self.parse_group_by()?;
        let having = self.parse_having(placeholder_counter)?;
        if having.is_some() && group_by.is_empty() {
            // `HAVING` without `GROUP BY` applies to the single aggregate group.
        }
        // `OFFSET` without `LIMIT` is a parse error (legacy rule).
        if matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("OFFSET")) {
            return Err(SqlError::parse("OFFSET without LIMIT is not supported"));
        }
        let order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit(placeholder_counter)?;
        let count_star = !star
            && !distinct
            && joins.is_empty()
            && group_by.is_empty()
            && having.is_none()
            && items.len() == 1
            && items[0].agg == Some(AggFunc::Count)
            && items[0].col == "*";
        // Legacy `columns` shape: empty for `*` and `COUNT(*)`, output names otherwise.
        let legacy_columns = if star || count_star { Vec::new() } else { columns };
        Ok(Stmt::Select {
            distinct,
            items,
            columns: legacy_columns,
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
        })
    }

    fn finish_union(&mut self, left: Stmt, placeholder_counter: &mut usize) -> Result<Stmt> {
        let mut acc = left;
        while self.eat_word("UNION") {
            let all = self.eat_word("ALL");
            let select_word = self.expect_word()?;
            if !select_word.eq_ignore_ascii_case("SELECT") {
                return Err(SqlError::parse(format!("expected SELECT after UNION, got {select_word}")));
            }
            let right = self.parse_select_body(&select_word, placeholder_counter)?;
            acc = Stmt::Union {
                left: Box::new(acc),
                right: Box::new(right),
                all,
            };
        }
        Ok(acc)
    }

    fn parse_limit(
        &mut self,
        placeholder_counter: &mut usize,
    ) -> Result<(Option<LimitValue>, Option<LimitValue>)> {
        if !self.eat_word("LIMIT") {
            return Ok((None, None));
        }
        let limit = Some(self.parse_limit_value(placeholder_counter)?);
        let offset = if self.eat_word("OFFSET") {
            Some(self.parse_limit_value(placeholder_counter)?)
        } else {
            None
        };
        Ok((limit, offset))
    }

    fn parse_limit_value(&mut self, placeholder_counter: &mut usize) -> Result<LimitValue> {
        match self.next() {
            Some(Token::Integer(v)) => Ok(LimitValue::Literal(v)),
            Some(Token::Question) => {
                *placeholder_counter += 1;
                Ok(LimitValue::Placeholder(*placeholder_counter))
            }
            Some(Token::Placeholder(n)) => {
                *placeholder_counter = (*placeholder_counter).max(n);
                Ok(LimitValue::Placeholder(n))
            }
            Some(Token::Named(_)) => {
                *placeholder_counter += 1;
                Ok(LimitValue::Placeholder(*placeholder_counter))
            }
            other => Err(SqlError::parse(format!("expected LIMIT value, got {other:?}"))),
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt> {
        let word = self.expect_word()?;
        match word.to_ascii_uppercase().as_str() {
            "PRAGMA" => {
                let name = self.expect_word()?;
                let mut value: Option<String> = None;
                if matches!(self.peek(), Some(Token::Eq)) {
                    self.pos += 1;
                    value = Some(self.pragma_value()?);
                } else if matches!(self.peek(), Some(Token::LParen)) {
                    self.pos += 1;
                    value = Some(self.pragma_value()?);
                    match self.next() {
                        Some(Token::RParen) => {}
                        other => return Err(SqlError::parse(format!("expected ')', got {other:?}"))),
                    }
                }
                Ok(Stmt::Pragma { name, value })
            }
            "CREATE" => {
                if self.eat_word("TABLE") {
                    let mut if_not_exists = false;
                    if self.eat_word("IF") {
                        if !self.eat_word("NOT") || !self.eat_word("EXISTS") {
                            return Err(SqlError::parse("expected IF NOT EXISTS"));
                        }
                        if_not_exists = true;
                    }
                    let name = self.expect_word()?;
                    match self.next() {
                        Some(Token::LParen) => {}
                        other => return Err(SqlError::parse(format!("expected '(', got {other:?}"))),
                    }
                    let mut columns = Vec::new();
                    loop {
                        if matches!(self.peek(), Some(Token::RParen)) {
                            self.pos += 1;
                            break;
                        }
                        let col_name = self.expect_word()?;
                        let coltype = self.expect_word()?.to_ascii_uppercase();
                        let mut primary_key = false;
                        let mut not_null = false;
                        let mut default: Option<Value> = None;
                        loop {
                            if self.eat_word("PRIMARY") {
                                if !self.eat_word("KEY") {
                                    return Err(SqlError::parse("expected KEY after PRIMARY"));
                                }
                                primary_key = true;
                            } else if self.eat_word("NOT") {
                                if !self.eat_word("NULL") {
                                    return Err(SqlError::parse("expected NULL after NOT"));
                                }
                                not_null = true;
                            } else if self.eat_word("DEFAULT") {
                                default = Some(self.parse_default_literal()?);
                            } else {
                                break;
                            }
                        }
                        columns.push(ColumnDef {
                            name: col_name,
                            coltype,
                            primary_key,
                            not_null,
                            default,
                        });
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.pos += 1;
                            }
                            Some(Token::RParen) => continue,
                            other => {
                                return Err(SqlError::parse(format!(
                                    "expected ',' or ')', got {other:?}"
                                )))
                            }
                        }
                    }
                    Ok(Stmt::CreateTable {
                        name,
                        columns,
                        if_not_exists,
                    })
                } else if self.eat_word("INDEX") {
                    let mut if_not_exists = false;
                    if self.eat_word("IF") {
                        if !self.eat_word("NOT") || !self.eat_word("EXISTS") {
                            return Err(SqlError::parse("expected IF NOT EXISTS"));
                        }
                        if_not_exists = true;
                    }
                    let name = self.expect_word()?;
                    if !self.eat_word("ON") {
                        return Err(SqlError::parse("expected ON in CREATE INDEX"));
                    }
                    let table = self.expect_word()?;
                    match self.next() {
                        Some(Token::LParen) => {}
                        other => return Err(SqlError::parse(format!("expected '(', got {other:?}"))),
                    }
                    let mut columns = Vec::new();
                    loop {
                        columns.push(self.expect_word()?);
                        match self.next() {
                            Some(Token::Comma) => continue,
                            Some(Token::RParen) => break,
                            other => {
                                return Err(SqlError::parse(format!(
                                    "expected ',' or ')', got {other:?}"
                                )))
                            }
                        }
                    }
                    Ok(Stmt::CreateIndex {
                        name,
                        table,
                        columns,
                        if_not_exists,
                    })
                } else {
                    Err(SqlError::parse("expected TABLE or INDEX after CREATE"))
                }
            }
            "INSERT" => {
                let mut or_replace = false;
                let mut or_ignore = false;
                if self.eat_word("OR") {
                    if self.eat_word("REPLACE") {
                        or_replace = true;
                    } else if self.eat_word("IGNORE") {
                        or_ignore = true;
                    } else {
                        return Err(SqlError::parse("expected REPLACE or IGNORE after OR"));
                    }
                }
                if !self.eat_word("INTO") {
                    return Err(SqlError::parse("expected INTO after INSERT"));
                }
                let table = self.expect_word()?;
                let mut columns = Vec::new();
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.pos += 1;
                    loop {
                        columns.push(self.expect_word()?);
                        match self.next() {
                            Some(Token::Comma) => continue,
                            Some(Token::RParen) => break,
                            other => {
                                return Err(SqlError::parse(format!(
                                    "expected ',' or ')', got {other:?}"
                                )))
                            }
                        }
                    }
                }
                if !self.eat_word("VALUES") {
                    return Err(SqlError::unsupported("only INSERT ... VALUES is supported"));
                }
                let mut rows = Vec::new();
                let mut counter = 0usize;
                loop {
                    match self.next() {
                        Some(Token::LParen) => {}
                        other => return Err(SqlError::parse(format!("expected '(', got {other:?}"))),
                    }
                    let mut row = Vec::new();
                    loop {
                        row.push(self.parse_expr(&mut counter)?);
                        match self.next() {
                            Some(Token::Comma) => continue,
                            Some(Token::RParen) => break,
                            other => {
                                return Err(SqlError::parse(format!(
                                    "expected ',' or ')', got {other:?}"
                                )))
                            }
                        }
                    }
                    rows.push(row);
                    if matches!(self.peek(), Some(Token::Comma)) {
                        self.pos += 1;
                        continue;
                    }
                    break;
                }
                Ok(Stmt::Insert {
                    or_replace,
                    or_ignore,
                    table,
                    columns,
                    rows,
                })
            }
            "SELECT" => {
                let mut counter = 0usize;
                let first = self.parse_select_body(&word, &mut counter)?;
                self.finish_union(first, &mut counter)
            }
            "ALTER" => {
                if !self.eat_word("TABLE") {
                    return Err(SqlError::parse("expected TABLE after ALTER"));
                }
                let table = self.expect_word()?;
                if !self.eat_word("ADD") {
                    return Err(SqlError::parse("expected ADD in ALTER TABLE"));
                }
                self.eat_word("COLUMN");
                let col_name = self.expect_word()?;
                let coltype = self.expect_word()?.to_ascii_uppercase();
                let mut not_null = false;
                let mut default: Option<Value> = None;
                loop {
                    if self.eat_word("NOT") {
                        if !self.eat_word("NULL") {
                            return Err(SqlError::parse("expected NULL after NOT"));
                        }
                        not_null = true;
                    } else if self.eat_word("DEFAULT") {
                        default = Some(self.parse_default_literal()?);
                    } else if self.eat_word("PRIMARY") {
                        return Err(SqlError::parse("PRIMARY KEY is not allowed in ADD COLUMN"));
                    } else {
                        break;
                    }
                }
                Ok(Stmt::AlterTable {
                    table,
                    column: ColumnDef {
                        name: col_name,
                        coltype,
                        primary_key: false,
                        not_null,
                        default,
                    },
                })
            }
            "UPDATE" => {
                let table = self.expect_word()?;
                if !self.eat_word("SET") {
                    return Err(SqlError::parse("expected SET in UPDATE"));
                }
                let mut assignments = Vec::new();
                let mut counter = 0usize;
                loop {
                    let col = self.expect_word()?;
                    match self.next() {
                        Some(Token::Eq) => {}
                        other => return Err(SqlError::parse(format!("expected '=', got {other:?}"))),
                    }
                    let expr = self.parse_expr(&mut counter)?;
                    assignments.push((col, expr));
                    if !matches!(self.peek(), Some(Token::Comma)) {
                        break;
                    }
                    self.pos += 1;
                }
                let filter = if self.eat_word("WHERE") {
                    Some(self.parse_where(&mut counter)?)
                } else {
                    None
                };
                Ok(Stmt::Update {
                    table,
                    assignments,
                    filter,
                })
            }
            "DELETE" => {
                if !self.eat_word("FROM") {
                    return Err(SqlError::parse("expected FROM after DELETE"));
                }
                let table = self.expect_word()?;
                let mut counter = 0usize;
                let filter = if self.eat_word("WHERE") {
                    Some(self.parse_where(&mut counter)?)
                } else {
                    None
                };
                Ok(Stmt::Delete { table, filter })
            }
            "BEGIN" => Ok(Stmt::Begin),
            "COMMIT" => Ok(Stmt::Commit),
            "ROLLBACK" => Ok(Stmt::Rollback),
            other => Err(SqlError::unsupported(format!("statement: {other}"))),
        }
    }

    fn pragma_value(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Word(w)) => Ok(w),
            Some(Token::Integer(v)) => Ok(v.to_string()),
            Some(Token::Real(v)) => Ok(v.to_string()),
            Some(Token::Text(s)) => Ok(s),
            other => Err(SqlError::parse(format!("expected pragma value, got {other:?}"))),
        }
    }
}

/// Split SQL text into individual statements on `;`, respecting quotes.
pub fn split_batch(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if in_single && chars.peek() == Some(&'\'') {
                current.push('\'');
                current.push(chars.next().unwrap());
                continue;
            }
            in_single = !in_single;
            current.push(c);
            continue;
        }
        if c == ';' && !in_single {
            if !current.trim().is_empty() {
                out.push(current.trim().to_owned());
            }
            current = String::new();
            continue;
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

/// Parse exactly one statement; errors with `MultipleStatement` on batches.
pub fn parse_one(sql: &str) -> Result<Stmt> {
    let parts = split_batch(sql);
    if parts.is_empty() {
        return Err(SqlError::parse("empty statement"));
    }
    if parts.len() > 1 {
        return Err(SqlError::MultipleStatement);
    }
    let tokens = tokenize(&parts[0])?;
    let mut parser = Parser::new(tokens);
    let stmt = parser.parse_stmt()?;
    if parser.peek().is_some() {
        return Err(SqlError::parse("trailing tokens after statement"));
    }
    Ok(stmt)
}

/// Parse a batch of statements (used by `execute_batch`).
pub fn parse_batch(sql: &str) -> Result<Vec<Stmt>> {
    let mut out = Vec::new();
    for part in split_batch(sql) {
        let tokens = tokenize(&part)?;
        let mut parser = Parser::new(tokens);
        out.push(parser.parse_stmt()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(sql: &str) -> Stmt {
        parse_one(sql).expect("statement should parse")
    }

    #[test]
    fn comparison_operators_parse() {
        let cases = [
            ("SELECT a FROM t WHERE a != 1", CmpOp::NotEq),
            ("SELECT a FROM t WHERE a <> 1", CmpOp::NotEq),
            ("SELECT a FROM t WHERE a < 1", CmpOp::Lt),
            ("SELECT a FROM t WHERE a <= 1", CmpOp::LtEq),
            ("SELECT a FROM t WHERE a > 1", CmpOp::Gt),
            ("SELECT a FROM t WHERE a >= 1", CmpOp::GtEq),
        ];
        for (sql, op) in cases {
            match select(sql) {
                Stmt::Select { filter: Some(WhereClause::Cmp { col, op: found, .. }), .. } => {
                    assert_eq!(col, "a");
                    assert_eq!(found, op, "wrong operator for {sql}");
                }
                other => panic!("unexpected parse for {sql}: {other:?}"),
            }
        }
    }

    #[test]
    fn like_in_and_null_parse() {
        match select("SELECT a FROM t WHERE name LIKE 'A%'") {
            Stmt::Select { filter: Some(WhereClause::Like { col, negate, .. }), .. } => {
                assert_eq!(col, "name");
                assert!(!negate);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE age IN (1, 2, ?)") {
            Stmt::Select { filter: Some(WhereClause::In { col, values, negate }), .. } => {
                assert_eq!(col, "age");
                assert!(!negate);
                assert_eq!(values.len(), 3);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE nick IS NULL") {
            Stmt::Select { filter: Some(WhereClause::IsNull(col)), .. } => assert_eq!(col, "nick"),
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE nick IS NOT NULL") {
            Stmt::Select { filter: Some(WhereClause::IsNotNull(col)), .. } => {
                assert_eq!(col, "nick")
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE nick NOT LIKE 'x%'") {
            Stmt::Select { filter: Some(WhereClause::Like { negate, .. }), .. } => assert!(negate),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn or_chains_with_and_precedence() {
        match select("SELECT a FROM t WHERE a = 1 OR b = 2 AND c = 3") {
            Stmt::Select { filter: Some(WhereClause::Or(items)), .. } => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[1], WhereClause::And(_)));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn order_by_limit_offset_parse() {
        match select("SELECT a FROM t ORDER BY b DESC, c ASC LIMIT 10 OFFSET 5") {
            Stmt::Select { order_by, limit, offset, count_star, .. } => {
                assert_eq!(
                    order_by,
                    vec![
                        OrderBy { col: "b".into(), desc: true },
                        OrderBy { col: "c".into(), desc: false },
                    ]
                );
                assert_eq!(limit, Some(LimitValue::Literal(10)));
                assert_eq!(offset, Some(LimitValue::Literal(5)));
                assert!(!count_star);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn count_star_parses_with_filter() {
        match select("SELECT COUNT(*) FROM t WHERE a > 1") {
            Stmt::Select { count_star, filter, .. } => {
                assert!(count_star);
                assert!(filter.is_some());
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn named_params_bind_in_order() {
        match select("SELECT a FROM t WHERE a = :first AND b = @second") {
            Stmt::Select { filter: Some(WhereClause::And(items)), .. } => {
                assert_eq!(
                    items,
                    vec![
                        WhereClause::Eq("a".into(), Expr::Placeholder(1)),
                        WhereClause::Eq("b".into(), Expr::Placeholder(2)),
                    ]
                );
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn like_matcher_wildcards() {
        assert!(like_match("Alice", "A%"));
        assert!(like_match("Abcde", "A_c%"));
        assert!(!like_match("Alice", "A_c%"));
        assert!(like_match("abc", "a_c"));
        assert!(!like_match("ac", "a_c"));
        assert!(like_match("anything", "%"));
        assert!(!like_match("abc", "abd"));
    }

    #[test]
    fn join_parses_inner_left_and_chained() {
        match select("SELECT a.x, b.y FROM a INNER JOIN b ON a.x = b.y") {
            Stmt::Select { joins, items, .. } => {
                assert_eq!(joins.len(), 1);
                assert_eq!(joins[0].kind, JoinKind::Inner);
                assert_eq!(joins[0].table, "b");
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].table.as_deref(), Some("a"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a.x FROM a LEFT JOIN b ON a.x = b.y") {
            Stmt::Select { joins, .. } => assert_eq!(joins[0].kind, JoinKind::Left),
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a.x FROM a JOIN b ON a.x = b.y JOIN c ON b.y = c.z") {
            Stmt::Select { joins, .. } => assert_eq!(joins.len(), 2),
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(parse_one("SELECT a FROM a JOIN b ON a.x > b.y").is_err());
        assert!(parse_one("SELECT a FROM a JOIN b").is_err());
        assert!(parse_one("SELECT a FROM a INNER b ON a.x = b.y").is_err());
    }

    #[test]
    fn subselect_parses_in_and_scalar() {
        match select("SELECT a FROM t WHERE a IN (SELECT b FROM u WHERE c = 1)") {
            Stmt::Select { filter: Some(WhereClause::InSelect { col, negate, .. }), .. } => {
                assert_eq!(col, "a");
                assert!(!negate);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE a = (SELECT b FROM u)") {
            Stmt::Select { filter: Some(WhereClause::CmpSelect { col, op, .. }), .. } => {
                assert_eq!(col, "a");
                assert_eq!(op, CmpOp::Eq);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a FROM t WHERE a NOT IN (SELECT b FROM u)") {
            Stmt::Select { filter: Some(WhereClause::InSelect { negate, .. }), .. } => assert!(negate),
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(parse_one("SELECT a FROM t WHERE a IN ()").is_err());
        assert!(parse_one("SELECT a FROM t WHERE a IN (SELECT)").is_err());
    }

    #[test]
    fn aggregates_parse_without_group() {
        for (sql, func, col) in [
            ("SELECT COUNT(*) FROM t", AggFunc::Count, "*"),
            ("SELECT COUNT(a) FROM t", AggFunc::Count, "a"),
            ("SELECT SUM(a) FROM t", AggFunc::Sum, "a"),
            ("SELECT AVG(a) FROM t", AggFunc::Avg, "a"),
            ("SELECT MIN(a) FROM t", AggFunc::Min, "a"),
            ("SELECT MAX(a) FROM t", AggFunc::Max, "a"),
        ] {
            match select(sql) {
                Stmt::Select { items, .. } => {
                    assert_eq!(items.len(), 1, "wrong items for {sql}");
                    assert_eq!(items[0].agg, Some(func), "wrong func for {sql}");
                    assert_eq!(items[0].col, col, "wrong col for {sql}");
                }
                other => panic!("unexpected parse for {sql}: {other:?}"),
            }
        }
        assert!(parse_one("SELECT SUM(*) FROM t").is_err());
        assert!(parse_one("SELECT COUNT() FROM t").is_err());
        // Legacy COUNT(*) flag keeps working.
        match select("SELECT COUNT(*) FROM t") {
            Stmt::Select { count_star, .. } => assert!(count_star),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn group_by_having_parse() {
        match select("SELECT d, COUNT(*) FROM t GROUP BY d HAVING COUNT(*) > 1") {
            Stmt::Select { group_by, having, .. } => {
                assert_eq!(group_by, vec!["d".to_string()]);
                assert!(matches!(having, Some(HavingClause::Cond(_))));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT a, b FROM t GROUP BY a, b HAVING a = 1 AND COUNT(*) > 2") {
            Stmt::Select { group_by, having: Some(HavingClause::And(items)), .. } => {
                assert_eq!(group_by.len(), 2);
                assert_eq!(items.len(), 2);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(parse_one("SELECT a FROM t GROUP BY").is_err());
        assert!(parse_one("SELECT a FROM t HAVING").is_err());
        assert!(parse_one("SELECT a FROM t HAVING COUNT(*)").is_err());
    }

    #[test]
    fn distinct_parses() {
        match select("SELECT DISTINCT a FROM t") {
            Stmt::Select { distinct, items, .. } => {
                assert!(distinct);
                assert_eq!(items.len(), 1);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match select("SELECT DISTINCT * FROM t") {
            Stmt::Select { distinct, star, .. } => {
                assert!(distinct);
                assert!(star);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // DISTINCT COUNT(*) is accepted (no-op dedup over one row).
        match select("SELECT DISTINCT COUNT(*) FROM t") {
            Stmt::Select { distinct, items, .. } => {
                assert!(distinct);
                assert_eq!(items[0].agg, Some(AggFunc::Count));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn union_parses_all_and_plain() {
        match parse_one("SELECT a FROM t UNION SELECT a FROM u").unwrap() {
            Stmt::Union { all, .. } => assert!(!all),
            other => panic!("unexpected parse: {other:?}"),
        }
        match parse_one("SELECT a FROM t UNION ALL SELECT a FROM u").unwrap() {
            Stmt::Union { all, .. } => assert!(all),
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(parse_one("SELECT a FROM t UNION").is_err());
        assert!(parse_one("SELECT a FROM t UNION SELECT").is_err());
    }

    #[test]
    fn alter_table_parses_add_column() {
        match parse_one("ALTER TABLE t ADD COLUMN c TEXT NOT NULL DEFAULT 'x'").unwrap() {
            Stmt::AlterTable { table, column } => {
                assert_eq!(table, "t");
                assert_eq!(column.name, "c");
                assert!(column.not_null);
                assert_eq!(column.default, Some(Value::Text("x".into())));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match parse_one("ALTER TABLE t ADD COLUMN n INTEGER DEFAULT 5").unwrap() {
            Stmt::AlterTable { column, .. } => {
                assert_eq!(column.default, Some(Value::Integer(5)));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(parse_one("ALTER TABLE t ADD COLUMN c").is_err());
        assert!(parse_one("ALTER TABLE t ADD COLUMN c TEXT PRIMARY KEY").is_err());
    }
}
