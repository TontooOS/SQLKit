//! Error types for SQLKit, modeled on the `rusqlite::Error` surface.

use thiserror::Error;

/// Main error type for SQLKit operations.
///
/// The variants mirror the `rusqlite::Error` cases used by TontooOS
/// (`SqliteFailure`, `ExecuteReturnedResults`, `QueryReturnedNoRows`, ...),
/// plus engine-specific cases (`Parse`, `NotSqliteFile`, `Unsupported`).
#[derive(Error, Debug)]
pub enum SqlError {
    #[error("SQLite failure {code}: {message}")]
    SqliteFailure { code: i32, message: String },

    #[error("statement expected no rows but returned rows")]
    ExecuteReturnedResults,

    #[error("multiple statements given where only one is allowed")]
    MultipleStatement,

    #[error("invalid parameter count: expected {expected}, got {got}")]
    InvalidParameterCount { expected: usize, got: usize },

    #[error("invalid parameter name: {0}")]
    InvalidParameterName(String),

    #[error("value conversion to SQL failed: {0}")]
    ToSqlConversionFailure(String),

    #[error("value conversion from SQL failed: {0}")]
    FromSqlConversionFailure(String),

    #[error("query returned no rows")]
    QueryReturnedNoRows,

    #[error("invalid column index: {0}")]
    InvalidColumnIndex(usize),

    #[error("invalid column name: {0}")]
    InvalidColumnName(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("SQL parse error: {0}")]
    Parse(String),

    #[error("not a SQLite file: {0}")]
    NotSqliteFile(String),

    #[error("unsupported statement or feature: {0}")]
    Unsupported(String),

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("{0}")]
    Custom(String),
}

impl SqlError {
    pub fn custom(msg: impl Into<String>) -> Self {
        Self::Custom(msg.into())
    }

    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}

/// Common result type for SQLKit.
pub type Result<T> = std::result::Result<T, SqlError>;
