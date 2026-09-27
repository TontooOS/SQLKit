//! SQL value model plus `ToSql` / `FromValue` conversions and `params!`.

use crate::error::{Result, SqlError};
use serde::{Deserialize, Serialize};

/// A single SQL value.
///
/// Mirrors `rusqlite::types::Value` so callers can migrate from rusqlite
/// without rethinking their value handling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(v) => Some(*v),
            Value::Real(v) => Some(*v as i64),
            Value::Text(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Real(v) => Some(*v),
            Value::Integer(v) => Some(*v as f64),
            Value::Text(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            Value::Text(s) => Some(s.as_bytes()),
            _ => None,
        }
    }
}

/// Convert a Rust value into a [`Value`].
pub trait ToSql {
    fn to_sql(&self) -> Result<Value>;
}

impl ToSql for Value {
    fn to_sql(&self) -> Result<Value> {
        Ok(self.clone())
    }
}

impl ToSql for i32 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(*self as i64))
    }
}

impl ToSql for i64 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(*self))
    }
}

impl ToSql for u32 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(*self as i64))
    }
}

impl ToSql for usize {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(*self as i64))
    }
}

impl ToSql for f32 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Real(*self as f64))
    }
}

impl ToSql for f64 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Real(*self))
    }
}

impl ToSql for bool {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(i64::from(*self)))
    }
}

impl ToSql for String {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.clone()))
    }
}

impl ToSql for str {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.to_owned()))
    }
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self) -> Result<Value> {
        (**self).to_sql()
    }
}

impl ToSql for Vec<u8> {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.clone()))
    }
}

impl ToSql for [u8] {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.to_vec()))
    }
}

impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self) -> Result<Value> {
        match self {
            Some(v) => v.to_sql(),
            None => Ok(Value::Null),
        }
    }
}

/// Convert a [`Value`] back into a Rust type (used by `Row::get`).
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self>;
}

impl FromValue for Value {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(value.clone())
    }
}

impl FromValue for i32 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(*v as i32),
            Value::Real(v) => Ok(*v as i32),
            Value::Text(s) => s
                .parse()
                .map_err(|_| SqlError::FromSqlConversionFailure(format!("not an i32: {s}"))),
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not i32".into())),
            Value::Blob(_) => Err(SqlError::FromSqlConversionFailure("BLOB is not i32".into())),
        }
    }
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(*v),
            Value::Real(v) => Ok(*v as i64),
            Value::Text(s) => s
                .parse()
                .map_err(|_| SqlError::FromSqlConversionFailure(format!("not an i64: {s}"))),
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not i64".into())),
            Value::Blob(_) => Err(SqlError::FromSqlConversionFailure("BLOB is not i64".into())),
        }
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Real(v) => Ok(*v),
            Value::Integer(v) => Ok(*v as f64),
            Value::Text(s) => s
                .parse()
                .map_err(|_| SqlError::FromSqlConversionFailure(format!("not an f64: {s}"))),
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not f64".into())),
            Value::Blob(_) => Err(SqlError::FromSqlConversionFailure("BLOB is not f64".into())),
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(*v != 0),
            Value::Real(v) => Ok(*v != 0.0),
            Value::Text(s) => match s.as_str() {
                "1" | "true" | "TRUE" => Ok(true),
                "0" | "false" | "FALSE" => Ok(false),
                _ => Err(SqlError::FromSqlConversionFailure(format!("not a bool: {s}"))),
            },
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not bool".into())),
            Value::Blob(_) => Err(SqlError::FromSqlConversionFailure("BLOB is not bool".into())),
        }
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Text(s) => Ok(s.clone()),
            Value::Integer(v) => Ok(v.to_string()),
            Value::Real(v) => Ok(v.to_string()),
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not String".into())),
            Value::Blob(b) => String::from_utf8(b.clone())
                .map_err(|_| SqlError::FromSqlConversionFailure("BLOB is not valid UTF-8".into())),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Blob(b) => Ok(b.clone()),
            Value::Text(s) => Ok(s.as_bytes().to_vec()),
            Value::Integer(v) => Ok(v.to_le_bytes().to_vec()),
            Value::Real(v) => Ok(v.to_le_bytes().to_vec()),
            Value::Null => Err(SqlError::FromSqlConversionFailure("NULL is not Vec<u8>".into())),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            v => Ok(Some(T::from_value(v)?)),
        }
    }
}

/// Convert bound parameters into an owned `Vec<Value>`.
///
/// Implemented for `Vec<Value>`, fixed arrays of `Value`, `()` (no params),
/// and tuples up to 6 elements so `conn.execute(sql, params![...])`,
/// `conn.execute(sql, [])` and `conn.execute(sql, ())` all work.
pub trait IntoParams {
    fn into_params(self) -> Result<Vec<Value>>;
}

impl IntoParams for Vec<Value> {
    fn into_params(self) -> Result<Vec<Value>> {
        Ok(self)
    }
}

impl IntoParams for () {
    fn into_params(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}

impl<const N: usize> IntoParams for [Value; N] {
    fn into_params(self) -> Result<Vec<Value>> {
        Ok(self.into_iter().collect())
    }
}

macro_rules! impl_into_params_tuple {
    ($($t:ident),*) => {
        impl<$($t: ToSql),*> IntoParams for ($($t,)*) {
            #[allow(non_snake_case)]
            fn into_params(self) -> Result<Vec<Value>> {
                let ($($t,)*) = self;
                Ok(vec![$($t.to_sql()?),*])
            }
        }
    };
}

impl_into_params_tuple!(A);
impl_into_params_tuple!(A, B);
impl_into_params_tuple!(A, B, C);
impl_into_params_tuple!(A, B, C, D);
impl_into_params_tuple!(A, B, C, D, E);
impl_into_params_tuple!(A, B, C, D, E, F);

/// Build a `Vec<Value>` from Rust values, like `rusqlite::params!`.
///
/// ```rust
/// use sqlkit::{params, Value};
/// let p = params![1i32, "hello", vec![1u8, 2u8]];
/// assert_eq!(p.len(), 3);
/// ```
#[macro_export]
macro_rules! params {
    () => {
        Vec::<$crate::Value>::new()
    };
    ($($value:expr),* $(,)?) => {
        vec![$($crate::ToSql::to_sql(&$value).expect("sqlkit params! conversion"),)*]
    };
}
