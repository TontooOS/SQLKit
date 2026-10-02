//! Error types for SQLKit, modeled on the `rusqlite::Error` surface.

/// Main error type for SQLKit operations.
///
/// The variants mirror the `rusqlite::Error` cases used by TontooOS
/// (`SqliteFailure`, `ExecuteReturnedResults`, `QueryReturnedNoRows`, ...),
/// plus engine-specific cases (`Parse`, `NotSqliteFile`, `Unsupported`).
#[derive(Debug)]
pub enum SqlError {
    SqliteFailure { code: i32, message: String },

    ExecuteReturnedResults,

    MultipleStatement,

    InvalidParameterCount { expected: usize, got: usize },

    InvalidParameterName(String),

    ToSqlConversionFailure(String),

    FromSqlConversionFailure(String),

    QueryReturnedNoRows,

    InvalidColumnIndex(usize),

    InvalidColumnName(String),

    InvalidPath(String),

    Io(std::io::Error),

    Serde(String),

    Parse(String),

    NotSqliteFile(String),

    Unsupported(String),

    Transaction(String),

    Custom(String),
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlError::SqliteFailure { code, message } => {
                write!(f, "SQLite failure {code}: {message}")
            }
            SqlError::ExecuteReturnedResults => {
                f.write_str("statement expected no rows but returned rows")
            }
            SqlError::MultipleStatement => {
                f.write_str("multiple statements given where only one is allowed")
            }
            SqlError::InvalidParameterCount { expected, got } => {
                write!(f, "invalid parameter count: expected {expected}, got {got}")
            }
            SqlError::InvalidParameterName(name) => {
                write!(f, "invalid parameter name: {name}")
            }
            SqlError::ToSqlConversionFailure(why) => {
                write!(f, "value conversion to SQL failed: {why}")
            }
            SqlError::FromSqlConversionFailure(why) => {
                write!(f, "value conversion from SQL failed: {why}")
            }
            SqlError::QueryReturnedNoRows => f.write_str("query returned no rows"),
            SqlError::InvalidColumnIndex(idx) => write!(f, "invalid column index: {idx}"),
            SqlError::InvalidColumnName(name) => write!(f, "invalid column name: {name}"),
            SqlError::InvalidPath(path) => write!(f, "invalid path: {path}"),
            SqlError::Io(e) => write!(f, "I/O error: {e}"),
            SqlError::Serde(why) => write!(f, "serialization error: {why}"),
            SqlError::Parse(why) => write!(f, "SQL parse error: {why}"),
            SqlError::NotSqliteFile(why) => write!(f, "not a SQLite file: {why}"),
            SqlError::Unsupported(why) => write!(f, "unsupported statement or feature: {why}"),
            SqlError::Transaction(why) => write!(f, "transaction error: {why}"),
            SqlError::Custom(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SqlError {
    fn from(e: std::io::Error) -> Self {
        SqlError::Io(e)
    }
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

/// Maximum characters of untrusted text echoed inside an error message.
///
/// Longer values are cut off with a `...[truncated]` suffix so error paths
/// never leak secrets or blow up logs with megabytes of echoed input.
pub const MAX_ERROR_ECHO_CHARS: usize = 128;

/// Shorten untrusted text for error messages. Short inputs pass through
/// unchanged; anything longer is cut to [`MAX_ERROR_ECHO_CHARS`] characters.
pub(crate) fn redact(text: &str) -> String {
    if text.chars().count() <= MAX_ERROR_ECHO_CHARS {
        text.to_owned()
    } else {
        let short: String = text.chars().take(MAX_ERROR_ECHO_CHARS).collect();
        format!("{short}...[truncated]")
    }
}

/// Compatibility helper mirroring `rusqlite::OptionalExtension`: converts
/// `QueryReturnedNoRows` into `Ok(None)` for optional single-row lookups.
///
/// ```rust
/// use sqlkit::{params, Connection, OptionalExtension};
/// let conn = Connection::open_in_memory().unwrap();
/// conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY)").unwrap();
/// let hit: Option<String> = conn
///     .query_row("SELECT id FROM t WHERE id = ?1", params!["x"], |row| row.get(0))
///     .optional()
///     .unwrap();
/// assert_eq!(hit, None);
/// ```
pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(SqlError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Common result type for SQLKit.
pub type Result<T> = std::result::Result<T, SqlError>;
