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

/// `col = expr` condition, chained with AND.
#[derive(Clone, Debug, PartialEq)]
pub enum WhereClause {
    Eq(String, Expr),
    And(Vec<WhereClause>),
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
            WhereClause::And(items) => {
                for item in items {
                    if !item.matches(columns, row, bound)? {
                        return Ok(false);
                    }
                }
                Ok(true)
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
    Question,
    Eq,
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
        match self.next() {
            Some(Token::Eq) => {}
            other => return Err(SqlError::parse(format!("expected '=', got {other:?}"))),
        }
        let expr = self.parse_expr(placeholder_counter)?;
        Ok(WhereClause::Eq(col, expr))
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
                Ok(Stmt::Select {
                    columns,
                    star,
                    table,
                    filter,
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
