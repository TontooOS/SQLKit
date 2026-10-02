//! Snapshot JSON codec for [`Value`], [`Column`] and [`Table`].
//!
//! The legacy `TSQL01` snapshot format is a plain JSON document, so files
//! written by the old `serde_json` backend must stay byte-for-byte readable.
//! These converters therefore mirror `serde_json`'s default encoding exactly:
//!
//! - `Value::Blob` becomes an array of integers (`[1,2,3]`), not a string.
//! - `Option::None` is written as an explicit `null` (no `skip_serializing_if`).
//! - Struct members keep declaration order.
//!
//! Parsing and rendering go through `foundation::serialization::JsonValue`;
//! SQLKit has no third-party dependencies.

use crate::connection::{Column, Table};
use crate::error::{Result, SqlError};
use crate::value::Value;
use foundation::serialization::JsonValue;
use std::collections::HashMap;

fn bad(what: &str) -> SqlError {
    SqlError::Serde(format!("invalid snapshot JSON: {what}"))
}

/// `Value` to JSON. Blob becomes an integer array, matching `serde_json`.
pub fn value_to_json(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Integer(i) => JsonValue::Integer(*i),
        Value::Real(f) => JsonValue::Float(*f),
        Value::Text(s) => JsonValue::Str(s.clone()),
        Value::Blob(b) => {
            JsonValue::Array(b.iter().map(|byte| JsonValue::Integer(i64::from(*byte))).collect())
        }
    }
}

/// JSON to `Value`. Integer arrays become blobs; objects and bools are errors.
pub fn value_from_json(json: &JsonValue) -> Result<Value> {
    Ok(match json {
        JsonValue::Null => Value::Null,
        JsonValue::Integer(i) => Value::Integer(*i),
        JsonValue::Float(f) => Value::Real(*f),
        JsonValue::Str(s) => Value::Text(s.clone()),
        JsonValue::Array(items) => {
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::Integer(i) if (0..=255).contains(i) => bytes.push(*i as u8),
                    _ => return Err(bad("BLOB must be an array of byte integers")),
                }
            }
            Value::Blob(bytes)
        }
        JsonValue::Bool(_) => return Err(bad("bool is not a SQL value")),
        JsonValue::Object(_) => return Err(bad("object is not a SQL value")),
    })
}

fn row_to_json(row: &[Value]) -> JsonValue {
    JsonValue::Array(row.iter().map(value_to_json).collect())
}

fn row_from_json(json: &JsonValue) -> Result<Vec<Value>> {
    let items = json
        .as_array()
        .ok_or_else(|| bad("row must be an array"))?;
    items.iter().map(value_from_json).collect()
}

/// `Column` to JSON, members in declaration order.
pub fn column_to_json(column: &Column) -> JsonValue {
    JsonValue::Object(vec![
        ("name".to_string(), JsonValue::Str(column.name.clone())),
        ("coltype".to_string(), JsonValue::Str(column.coltype.clone())),
        ("primary_key".to_string(), JsonValue::Bool(column.primary_key)),
        ("not_null".to_string(), JsonValue::Bool(column.not_null)),
        (
            "default".to_string(),
            column.default.as_ref().map_or(JsonValue::Null, value_to_json),
        ),
    ])
}

/// JSON to `Column`. `default` is optional (matches `#[serde(default)]`).
pub fn column_from_json(json: &JsonValue) -> Result<Column> {
    let text = |key: &str| -> Result<String> {
        json.get(key)
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| bad(&format!("column.{key} must be a string")))
    };
    let flag = |key: &str| -> Result<bool> {
        json.get(key)
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| bad(&format!("column.{key} must be a bool")))
    };
    let default = match json.get("default") {
        None | Some(JsonValue::Null) => None,
        Some(value) => Some(value_from_json(value)?),
    };
    Ok(Column {
        name: text("name")?,
        coltype: text("coltype")?,
        primary_key: flag("primary_key")?,
        not_null: flag("not_null")?,
        default,
    })
}

/// `Table` to JSON, members in declaration order.
pub fn table_to_json(table: &Table) -> JsonValue {
    JsonValue::Object(vec![
        ("name".to_string(), JsonValue::Str(table.name.clone())),
        (
            "columns".to_string(),
            JsonValue::Array(table.columns.iter().map(column_to_json).collect()),
        ),
        (
            "rows".to_string(),
            JsonValue::Array(table.rows.iter().map(|row| row_to_json(row)).collect()),
        ),
    ])
}

/// JSON to `Table`.
pub fn table_from_json(json: &JsonValue) -> Result<Table> {
    let name = json
        .get("name")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad("table.name must be a string"))?;
    let columns = json
        .get("columns")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| bad("table.columns must be an array"))?
        .iter()
        .map(column_from_json)
        .collect::<Result<Vec<_>>>()?;
    let rows = json
        .get("rows")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| bad("table.rows must be an array"))?
        .iter()
        .map(row_from_json)
        .collect::<Result<Vec<_>>>()?;
    Ok(Table { name, columns, rows })
}

/// The whole table map to a JSON document (the snapshot body).
pub fn tables_to_json(tables: &HashMap<String, Table>) -> JsonValue {
    JsonValue::Object(
        tables
            .iter()
            .map(|(name, table)| (name.clone(), table_to_json(table)))
            .collect(),
    )
}

/// The whole table map from a JSON document (the snapshot body).
pub fn tables_from_json(json: &JsonValue) -> Result<HashMap<String, Table>> {
    let entries = json
        .object_entries()
        .ok_or_else(|| bad("snapshot root must be an object"))?;
    entries
        .iter()
        .map(|(name, table)| Ok((name.clone(), table_from_json(table)?)))
        .collect()
}

/// Parse a snapshot body into the table map.
pub fn parse_tables(bytes: &[u8]) -> Result<HashMap<String, Table>> {
    let text = std::str::from_utf8(bytes).map_err(|_| bad("snapshot body is not UTF-8"))?;
    tables_from_json(&JsonValue::parse(text).map_err(|e| bad(&e.to_string()))?)
}

/// Render the table map as a compact snapshot body.
pub fn write_tables(tables: &HashMap<String, Table>) -> String {
    tables_to_json(tables).stringify(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_is_an_integer_array_like_serde_json() {
        assert_eq!(
            value_to_json(&Value::Blob(vec![0, 1, 255])).stringify(false),
            "[0,1,255]"
        );
        assert_eq!(value_from_json(&JsonValue::parse("[0,1,255]").unwrap()).unwrap(), Value::Blob(vec![0, 1, 255]));
    }

    #[test]
    fn scalars_roundtrip() {
        for value in [
            Value::Null,
            Value::Integer(-42),
            Value::Real(1.5),
            Value::Real(2.0),
            Value::Text("hello".into()),
            Value::Blob(vec![]),
        ] {
            let back = value_from_json(&value_to_json(&value)).unwrap();
            assert_eq!(back, value);
        }
        assert_eq!(value_to_json(&Value::Real(2.0)).stringify(false), "2.0");
    }

    #[test]
    fn column_members_keep_declaration_order_and_null_default() {
        let column = Column {
            name: "id".into(),
            coltype: "TEXT".into(),
            primary_key: true,
            not_null: false,
            default: None,
        };
        assert_eq!(
            column_to_json(&column).stringify(false),
            r#"{"name":"id","coltype":"TEXT","primary_key":true,"not_null":false,"default":null}"#
        );
        assert_eq!(
            column_from_json(&column_to_json(&column)).unwrap().default,
            None
        );
    }

    #[test]
    fn column_default_may_be_absent() {
        let json = JsonValue::parse(
            r#"{"name":"a","coltype":"INTEGER","primary_key":false,"not_null":false}"#,
        )
        .unwrap();
        assert_eq!(column_from_json(&json).unwrap().default, None);
    }

    #[test]
    fn table_map_roundtrip() {
        let mut tables = HashMap::new();
        tables.insert(
            "t".to_string(),
            Table {
                name: "t".into(),
                columns: vec![Column {
                    name: "id".into(),
                    coltype: "TEXT".into(),
                    primary_key: true,
                    not_null: true,
                    default: Some(Value::Text("x".into())),
                }],
                rows: vec![vec![Value::Integer(1), Value::Real(0.5), Value::Null]],
            },
        );
        let body = write_tables(&tables);
        let back = parse_tables(body.as_bytes()).unwrap();
        assert_eq!(back.len(), 1);
        let table = back.get("t").unwrap();
        assert_eq!(table.name, "t");
        assert_eq!(table.columns.len(), 1);
        assert_eq!(table.columns[0].default, Some(Value::Text("x".into())));
        assert_eq!(table.rows, vec![vec![Value::Integer(1), Value::Real(0.5), Value::Null]]);
    }

    #[test]
    fn rejects_malformed_snapshots() {
        for bad in [
            "[]",
            r#"{"t":{"name":"t"}}"#,
            r#"{"t":{"name":"t","columns":[],"rows":[[{}]]}}"#,
            r#"{"t":{"name":"t","columns":[{"name":1}],"rows":[]}}"#,
        ] {
            assert!(parse_tables(bad.as_bytes()).is_err(), "should reject {bad}");
        }
    }
}