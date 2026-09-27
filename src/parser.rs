//! Minimal SQL parser for the basis engine.
//!
//! Supports the statement subset CoreData and the basis tests need:
//! `PRAGMA`, `CREATE TABLE`, `CREATE INDEX`, `INSERT`, `SELECT`, `UPDATE`,
//! `DELETE`, plus `BEGIN` / `COMMIT` / `ROLLBACK` inside `execute_batch`.
//! The full SQLite grammar (JOIN, sub-selects, triggers, views, ...) is an
//! explicit roadmap item for follow-up subagents.

use crate::error::{Result, SqlError};
use crate::value::Value;

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
    IsNull(String),
    IsNotNull(String),
    And(Vec<WhereClause>),
    Or(Vec<WhereClause>),
}

impl WhereClause {
    pub fn matches(&self, columns: &[String], row: &[Value], bound: &[Value]) -> Result<bool> {
        match self {
            WhereClause::Eq(col, expr) => {
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let expected = match expr {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                Ok(values_equal(&row[idx], &expected))
            }
            WhereClause::Cmp { col, op, expr } => {
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let expected = match expr {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                Ok(eval_cmp(&row[idx], &expected, *op))
            }
            WhereClause::Like { col, pattern, negate } => {
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                let pat = match pattern {
                    Expr::Placeholder(n) => bound.get(n - 1).cloned().unwrap_or(Value::Null),
                    Expr::Literal(v) => v.clone(),
                };
                let matched = eval_like(&row[idx], &pat);
                Ok(if *negate { !matched } else { matched })
            }
            WhereClause::In { col, values, negate } => {
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
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
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                Ok(row[idx].is_null())
            }
            WhereClause::IsNotNull(col) => {
                let idx = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(col))
                    .ok_or_else(|| SqlError::InvalidColumnName(col.clone()))?;
                Ok(!row[idx].is_null())
            }
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

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::Text(s) => Some(s.clone()),
        Value::Integer(v) => Some(v.to_string()),
        Value::Real(v) => Some(v.to_string()),
        Value::Blob(bytes) => String::from_utf8(bytes.clone()).ok(),
        Value::Null => None,
    }
}

pub(crate) fn eval_like(actual: &Value, pattern: &Value) -> bool {
    match (value_to_text(actual), value_to_text(pattern)) {
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

/// Column definition inside `CREATE TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub coltype: String,
    pub primary_key: bool,
    pub not_null: bool,
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
        columns: Vec<String>,
        star: bool,
        table: String,
        filter: Option<WhereClause>,
        order_by: Vec<OrderBy>,
        limit: Option<LimitValue>,
        offset: Option<LimitValue>,
        count_star: bool,
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

    fn parse_condition(&mut self, placeholder_counter: &mut usize) -> Result<WhereClause> {
        let col = self.expect_word()?;
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
            let values = self.parse_in_list(placeholder_counter)?;
            return Ok(WhereClause::In { col, values, negate: false });
        }
        let op = match self.next() {
            Some(Token::Eq) => None,
            Some(Token::NotEq) => Some(CmpOp::NotEq),
            Some(Token::Less) => Some(CmpOp::Lt),
            Some(Token::LessEq) => Some(CmpOp::LtEq),
            Some(Token::Greater) => Some(CmpOp::Gt),
            Some(Token::GreaterEq) => Some(CmpOp::GtEq),
            other => {
                return Err(SqlError::parse(format!("expected operator, got {other:?}")))
            }
        };
        let expr = self.parse_expr(placeholder_counter)?;
        match op {
            None => Ok(WhereClause::Eq(col, expr)),
            Some(op) => Ok(WhereClause::Cmp { col, op, expr }),
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
            let col = self.expect_word()?;
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
                                let _ = self.next();
                            } else {
                                break;
                            }
                        }
                        columns.push(ColumnDef {
                            name: col_name,
                            coltype,
                            primary_key,
                            not_null,
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
                if matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("COUNT")) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::LParen) => {}
                        other => {
                            return Err(SqlError::parse(format!(
                                "expected '(' after COUNT, got {other:?}"
                            )))
                        }
                    }
                    match self.next() {
                        Some(Token::Star) => {}
                        other => {
                            return Err(SqlError::parse(format!(
                                "expected '*' in COUNT(*), got {other:?}"
                            )))
                        }
                    }
                    match self.next() {
                        Some(Token::RParen) => {}
                        other => return Err(SqlError::parse(format!("expected ')', got {other:?}"))),
                    }
                    if !self.eat_word("FROM") {
                        return Err(SqlError::parse("expected FROM in SELECT"));
                    }
                    let table = self.expect_word()?;
                    let mut counter = 0usize;
                    let filter = if self.eat_word("WHERE") {
                        Some(self.parse_where(&mut counter)?)
                    } else {
                        None
                    };
                    let order_by = self.parse_order_by()?;
                    let (limit, offset) = self.parse_limit(&mut counter)?;
                    return Ok(Stmt::Select {
                        columns: Vec::new(),
                        star: false,
                        table,
                        filter,
                        order_by,
                        limit,
                        offset,
                        count_star: true,
                    });
                }
                let mut columns = Vec::new();
                let mut star = false;
                if matches!(self.peek(), Some(Token::Star)) {
                    self.pos += 1;
                    star = true;
                } else {
                    loop {
                        columns.push(self.expect_word()?);
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.pos += 1;
                            }
                            _ => break,
                        }
                    }
                }
                if !self.eat_word("FROM") {
                    return Err(SqlError::parse("expected FROM in SELECT"));
                }
                let table = self.expect_word()?;
                let mut counter = 0usize;
                let filter = if self.eat_word("WHERE") {
                    Some(self.parse_where(&mut counter)?)
                } else {
                    None
                };
                let order_by = self.parse_order_by()?;
                let (limit, offset) = self.parse_limit(&mut counter)?;
                Ok(Stmt::Select {
                    columns,
                    star,
                    table,
                    filter,
                    order_by,
                    limit,
                    offset,
                    count_star: false,
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
}
