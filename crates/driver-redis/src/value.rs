//! Replies as result sets. A list of plain values is one column; a list of lists, one column per
//! position; a map, a key and a value column; a single value, one row. Values nested deeper
//! than that are shown as JSON. Bulk strings are text when they are UTF-8 and bytes otherwise:
//! Redis keeps strings, so `"42"` stays the text it is.

use redis::Value as R;
use treasury_core::exec::{Column, ValueKind};
use treasury_core::value::{Blob, Json, Value};

/// What a reply becomes.
#[derive(Debug, PartialEq)]
pub enum Shape {
    /// A plain acknowledgement (`OK`): nothing to show but that it ran.
    Done,
    /// Rows to show.
    Rows {
        columns: Vec<Column>,
        rows: Vec<Vec<Value>>,
    },
}

fn bulk(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::Text(s.to_owned()),
        Err(_) => Value::Bytes(Blob::full(b.to_vec())),
    }
}

/// A reply value as JSON, for values nested inside others.
fn json(v: &R) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        R::Nil => J::Null,
        R::Int(n) => J::from(*n),
        R::Double(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        R::Boolean(b) => J::Bool(*b),
        R::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => J::String(s.to_owned()),
            Err(_) => J::String(treasury_core::json::base64(b)),
        },
        R::SimpleString(s) => J::String(s.clone()),
        R::VerbatimString { text, .. } => J::String(text.clone()),
        R::Okay => J::String("OK".into()),
        R::BigNumber(n) => J::String(String::from_utf8_lossy(n).into_owned()),
        R::Array(items) | R::Set(items) => J::Array(items.iter().map(json).collect()),
        R::Push { data, .. } => J::Array(data.iter().map(json).collect()),
        R::Map(pairs) => J::Object(pairs.iter().map(|(k, v)| (key_text(k), json(v))).collect()),
        R::Attribute { data, .. } => json(data),
        R::ServerError(e) => J::String(e.to_string()),
        // A kind of reply added after this was written.
        other => J::String(format!("{other:?}")),
    }
}

fn key_text(k: &R) -> String {
    match json(k) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// One cell: a scalar as itself, anything nested as JSON.
pub fn cell(v: &R) -> Value {
    match v {
        R::Nil => Value::Null,
        R::Int(n) => Value::I64(*n),
        R::Double(f) => Value::F64(*f),
        R::Boolean(b) => Value::Bool(*b),
        R::BulkString(b) => bulk(b),
        R::SimpleString(s) => Value::Text(s.clone()),
        R::VerbatimString { text, .. } => Value::Text(text.clone()),
        R::Okay => Value::Text("OK".into()),
        R::BigNumber(n) => Value::Decimal(String::from_utf8_lossy(n).into_owned()),
        R::Attribute { data, .. } => cell(data),
        R::ServerError(e) => Value::Text(e.to_string()),
        nested => Value::Json(Json {
            text: json(nested).to_string(),
            binary: false,
        }),
    }
}

/// The kind a column has when every value in it is of one kind; mixed columns are `Any`.
fn kind_of(rows: &[Vec<Value>], i: usize) -> (ValueKind, &'static str) {
    let mut kind = None;
    for v in rows.iter().filter_map(|r| r.get(i)) {
        let k = match v {
            Value::Null => continue,
            Value::I64(_) => (ValueKind::Integer, "integer"),
            Value::F64(_) => (ValueKind::Float, "double"),
            Value::Bool(_) => (ValueKind::Bool, "boolean"),
            Value::Text(_) => (ValueKind::Text, "string"),
            Value::Bytes(_) => (ValueKind::Binary, "bytes"),
            Value::Decimal(_) => (ValueKind::Decimal, "big number"),
            Value::Json(_) => (ValueKind::Json, "nested"),
            _ => (ValueKind::Any, "value"),
        };
        match kind {
            None => kind = Some(k),
            Some(prev) if prev == k => {}
            Some(_) => return (ValueKind::Any, "value"),
        }
    }
    kind.unwrap_or((ValueKind::Any, "value"))
}

/// Columns named `names`, each of the kind its values have.
pub fn columns(names: &[String], rows: &[Vec<Value>]) -> Vec<Column> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let (kind, type_name) = kind_of(rows, i);
            Column::new(n.clone(), type_name, kind)
        })
        .collect()
}

/// A cell from a reply value it consumes, so bulk strings keep their bytes rather than being
/// copied: a page of a large list is read without a second allocation per entry.
pub fn into_cell(v: R) -> Value {
    match v {
        R::BulkString(b) => match String::from_utf8(b) {
            Ok(s) => Value::Text(s),
            Err(e) => Value::Bytes(Blob::full(e.into_bytes())),
        },
        R::SimpleString(s) => Value::Text(s),
        R::VerbatimString { text, .. } => Value::Text(text),
        other => cell(&other),
    }
}

/// The rows of a page of plain entries (LRANGE, ZRANGE), consuming the reply: one column, or
/// one per position when every entry is a list (a member and its score).
pub fn page_rows(items: Vec<R>) -> Vec<Vec<Value>> {
    items
        .into_iter()
        .map(|i| match i {
            R::Array(inner) | R::Set(inner) => inner.into_iter().map(into_cell).collect(),
            other => vec![into_cell(other)],
        })
        .collect()
}

/// The rows of a list: one column, or one per position when every item is itself a list.
pub fn list_rows(items: &[R]) -> (Vec<String>, Vec<Vec<Value>>) {
    let all_lists = !items.is_empty() && items.iter().all(|i| matches!(i, R::Array(_) | R::Set(_)));
    if all_lists {
        let width = items
            .iter()
            .map(|i| match i {
                R::Array(v) | R::Set(v) => v.len(),
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        let rows = items
            .iter()
            .map(|i| {
                let inner = match i {
                    R::Array(v) | R::Set(v) => v.as_slice(),
                    _ => &[],
                };
                (0..width)
                    .map(|n| inner.get(n).map_or(Value::Null, cell))
                    .collect()
            })
            .collect();
        let names = (1..=width).map(|n| n.to_string()).collect();
        return (names, rows);
    }
    (
        vec!["value".to_owned()],
        items.iter().map(|i| vec![cell(i)]).collect(),
    )
}

/// The rows of a map: a key and a value column.
pub fn map_rows(pairs: &[(R, R)]) -> (Vec<String>, Vec<Vec<Value>>) {
    (
        vec!["key".to_owned(), "value".to_owned()],
        pairs.iter().map(|(k, v)| vec![cell(k), cell(v)]).collect(),
    )
}

/// What `reply` shows as.
pub fn shape(reply: &R) -> Shape {
    let (names, rows) = match reply {
        R::Okay => return Shape::Done,
        R::Attribute { data, .. } => return shape(data),
        R::Array(items) | R::Set(items) => list_rows(items),
        R::Push { data, .. } => list_rows(data),
        R::Map(pairs) => map_rows(pairs),
        scalar => (vec!["value".to_owned()], vec![vec![cell(scalar)]]),
    };
    Shape::Rows {
        columns: columns(&names, &rows),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> R {
        R::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn plain_lists_are_one_column() {
        let Shape::Rows { columns, rows } = shape(&R::Array(vec![b("a"), b("b")])) else {
            panic!()
        };
        assert_eq!(columns[0].name, "value");
        assert_eq!(columns[0].kind, ValueKind::Text);
        assert_eq!(rows, [[Value::Text("a".into())], [Value::Text("b".into())]]);
    }

    #[test]
    fn lists_of_lists_get_a_column_per_position() {
        let reply = R::Array(vec![R::Array(vec![
            R::Int(42),
            b("héllo"),
            R::Double(3.5),
            R::Boolean(true),
            R::BulkString(vec![0xff, 0x00]),
        ])]);
        let Shape::Rows { columns, rows } = shape(&reply) else {
            panic!()
        };
        assert_eq!(
            columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"]
        );
        assert_eq!(rows[0][0], Value::I64(42));
        assert_eq!(rows[0][4], Value::Bytes(Blob::full(vec![0xff, 0x00])));
    }

    #[test]
    fn pages_keep_the_bytes_they_read() {
        let rows = page_rows(vec![
            R::Array(vec![b("a"), R::Double(1.5)]),
            R::Array(vec![R::BulkString(vec![0xff]), R::Double(2.0)]),
        ]);
        assert_eq!(rows[0], [Value::Text("a".into()), Value::F64(1.5)]);
        assert_eq!(rows[1][0], Value::Bytes(Blob::full(vec![0xff])));
        assert_eq!(page_rows(vec![b("x")]), [[Value::Text("x".into())]]);
    }

    #[test]
    fn maps_are_keys_and_values_and_deeper_values_are_json() {
        let reply = R::Map(vec![
            (b("name"), b("ada")),
            (b("tags"), R::Array(vec![b("x"), R::Int(1)])),
        ]);
        let Shape::Rows { columns, rows } = shape(&reply) else {
            panic!()
        };
        assert_eq!(columns[1].kind, ValueKind::Any);
        assert_eq!(
            rows[1][1],
            Value::Json(Json {
                text: r#"["x",1]"#.into(),
                binary: false
            })
        );
        assert_eq!(shape(&R::Okay), Shape::Done);
        // Numbers in strings stay strings.
        let Shape::Rows { rows, .. } = shape(&b("42")) else {
            panic!()
        };
        assert_eq!(rows, [[Value::Text("42".into())]]);
    }
}
