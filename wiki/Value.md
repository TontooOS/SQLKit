# Value

SQL value model with conversions and bound-parameter helpers.

## Value

```rust
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}
```

Mirrors `rusqlite::types::Value`. Equality across `Integer`, `Real`, and numeric `Text` follows SQLite comparison for the filter subset.

### Helpers

```rust
pub fn is_null(&self) -> bool
pub fn as_i64(&self) -> Option<i64>
pub fn as_f64(&self) -> Option<f64>
pub fn as_str(&self) -> Option<&str>
pub fn as_bytes(&self) -> Option<&[u8]>
```

## ToSql

```rust
pub trait ToSql {
    fn to_sql(&self) -> Result<Value>;
}
```

Implemented for `Value`, `i32`, `i64`, `u32`, `usize`, `f32`, `f64`, `bool`, `String`, `str`, `&str`, `Vec<u8>`, `&[u8]`, and `Option<T: ToSql>`. Conversions for these types never fail.

## FromValue

```rust
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self>;
}
```

Implemented for `Value`, `i32`, `i64`, `f64`, `bool`, `String`, `Vec<u8>`, and `Option<T: FromValue>`.

- Returns `Err(SqlError::FromSqlConversionFailure)` on `NULL` into non-`Option` targets and on incompatible types.

## IntoParams

```rust
pub trait IntoParams {
    fn into_params(self) -> Result<Vec<Value>>;
}
```

Implemented for `Vec<Value>`, `[Value; N]`, `()`, and tuples up to 6 elements. `conn.execute(sql, [])` and `conn.execute(sql, ())` both mean no parameters.

## params Macro

```rust
let p = params![1i32, "hello", vec![1u8, 2u8]];
```

Builds `Vec<Value>` like `rusqlite::params!`. Panics only when a custom `ToSql` implementation returns `Err`; all built-in conversions succeed.

## Usage / Example

```rust
use sqlkit::{params, Connection, Value};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, data BLOB)").unwrap();
conn.execute("INSERT INTO t (id, name, data) VALUES (?1, ?2, ?3)", params![1i32, "n", Value::Null]).unwrap();
```

## Cross References

- [Statement.md](Statement.md) – row mapping with `FromValue`
- [Connection.md](Connection.md) – parameter binding in `execute`
