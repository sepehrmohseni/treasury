//! BSON as Treasury values, and Extended JSON commands as BSON documents.

use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Bson, Document, RawBsonRef, RawDocument, RawDocumentBuf};
use treasury_core::exec::{Column, ValueKind};
use treasury_core::value::{
    Blob, BsonSpecial, Decimal128, List, ListKind, Timestamp, TimestampTz, Value,
};

/// Binary values keep at most their first megabyte.
const MAX_INLINE_BYTES: usize = 1024 * 1024;

/// The trailing column that holds fields the first documents did not have.
pub(crate) const OTHER_FIELDS: &str = "…other fields";

/// Parses a command written as (canonical or relaxed) Extended JSON.
pub(crate) fn document(
    json: serde_json::Map<String, serde_json::Value>,
) -> Result<Document, String> {
    match Bson::try_from(serde_json::Value::Object(json)) {
        Ok(Bson::Document(d)) => Ok(d),
        Ok(_) => Err("A command is a document.".into()),
        Err(e) => Err(format!("The command is not valid Extended JSON: {e}")),
    }
}

pub(crate) fn convert(b: Bson) -> Value {
    match b {
        Bson::Null => Value::Null,
        Bson::Boolean(v) => Value::Bool(v),
        Bson::Int32(n) => Value::I64(i64::from(n)),
        Bson::Int64(n) => Value::I64(n),
        Bson::Double(f) => Value::F64(f),
        Bson::String(s) => Value::Text(s),
        Bson::Document(d) => Value::Document(d.into_iter().map(|(k, v)| (k, convert(v))).collect()),
        Bson::Array(items) => Value::List(List {
            kind: ListKind::Array,
            items: items.into_iter().map(convert).collect(),
        }),
        Bson::ObjectId(id) => Value::ObjectId(id.bytes()),
        Bson::DateTime(dt) => {
            let ms = dt.timestamp_millis();
            Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: ms.div_euclid(1_000),
                    nanos: u32::try_from(ms.rem_euclid(1_000)).unwrap_or(0) * 1_000_000,
                },
                offset_secs: 0,
            })
        }
        Bson::Decimal128(d) => Value::Decimal128(Decimal128 {
            bytes: d.bytes(),
            text: d.to_string(),
        }),
        Bson::Binary(bin) => match (bin.subtype, bin.bytes.len()) {
            (BinarySubtype::Uuid, 16) => {
                let mut u = [0u8; 16];
                u.copy_from_slice(&bin.bytes);
                Value::Uuid(u)
            }
            (BinarySubtype::Generic, _) => {
                let total = u64::try_from(bin.bytes.len()).ok();
                let complete = bin.bytes.len() <= MAX_INLINE_BYTES;
                let mut bytes = bin.bytes;
                bytes.truncate(MAX_INLINE_BYTES);
                Value::Bytes(Blob {
                    bytes,
                    total_len: total,
                    complete,
                })
            }
            (subtype, _) => Value::Bson(BsonSpecial::Binary {
                subtype: u8::from(subtype),
                bytes: bin.bytes,
            }),
        },
        Bson::RegularExpression(r) => Value::Bson(BsonSpecial::Regex {
            pattern: r.pattern,
            options: r.options,
        }),
        Bson::JavaScriptCode(code) => Value::Bson(BsonSpecial::JavaScript(code)),
        Bson::JavaScriptCodeWithScope(c) => Value::Bson(BsonSpecial::JavaScript(c.code)),
        Bson::Timestamp(t) => Value::Bson(BsonSpecial::Timestamp {
            time: t.time,
            increment: t.increment,
        }),
        Bson::Symbol(s) => Value::Bson(BsonSpecial::Symbol(s)),
        Bson::Undefined => Value::Bson(BsonSpecial::Undefined),
        Bson::MinKey => Value::Bson(BsonSpecial::MinKey),
        Bson::MaxKey => Value::Bson(BsonSpecial::MaxKey),
        other => Value::Text(other.to_string()),
    }
}

/// The logical kind of a value, for columns decided from sample documents.
pub(crate) fn kind_of(v: &Value) -> ValueKind {
    match v {
        Value::Bool(_) => ValueKind::Bool,
        Value::I64(_) => ValueKind::Integer,
        Value::F64(_) => ValueKind::Float,
        Value::Decimal128(_) => ValueKind::Decimal,
        Value::Text(_) => ValueKind::Text,
        Value::Bytes(_) => ValueKind::Binary,
        Value::TimestampTz(_) => ValueKind::TimestampTz,
        Value::Uuid(_) => ValueKind::Uuid,
        Value::ObjectId(_) => ValueKind::ObjectId,
        Value::List(_) => ValueKind::List,
        Value::Document(_) => ValueKind::Document,
        _ => ValueKind::Any,
    }
}

/// Columns for a result: `_id` first, then the top-level fields of the first documents in the
/// order first seen, then a column for fields later documents add.
pub(crate) fn columns(first: &[Document]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if first.iter().any(|d| d.contains_key("_id")) {
        names.push("_id".into());
    }
    for d in first {
        for k in d.keys() {
            if !names.iter().any(|n| n == k) {
                names.push(k.clone());
            }
        }
    }
    names.push(OTHER_FIELDS.into());
    names
}

/// The column descriptions, with each column's kind taken from the first non-null value.
pub(crate) fn describe(names: &[String], first_rows: &[Vec<Value>]) -> Vec<Column> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let kind = first_rows
                .iter()
                .filter_map(|r| r.get(i))
                .find(|v| !v.is_null())
                .map_or(ValueKind::Any, kind_of);
            let kind = if n == OTHER_FIELDS {
                ValueKind::Document
            } else {
                kind
            };
            Column::new(
                n,
                if kind == ValueKind::Document {
                    "document"
                } else {
                    "bson"
                },
                kind,
            )
        })
        .collect()
}

/// One document as a row of `columns`; fields not among them go into the last column.
pub(crate) fn row(names: &[String], doc: Document) -> Vec<Value> {
    let fields = names.len().saturating_sub(1);
    let mut out = vec![Value::Null; names.len()];
    let mut extra = Vec::new();
    for (k, v) in doc {
        match names[..fields].iter().position(|n| *n == k) {
            Some(i) => out[i] = convert(v),
            None => extra.push((k, convert(v))),
        }
    }
    if !extra.is_empty()
        && let Some(last) = out.last_mut()
    {
        *last = Value::Document(extra);
    }
    out
}

/// The fields of raw documents, as [`columns`] decides them for parsed ones.
pub(crate) fn raw_columns(first: &[RawDocumentBuf]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if first.iter().any(|d| d.get("_id").ok().flatten().is_some()) {
        names.push("_id".into());
    }
    for d in first {
        for (k, _) in d.iter().flatten() {
            if !names.iter().any(|n| n == k) {
                names.push(k.to_owned());
            }
        }
    }
    names.push(OTHER_FIELDS.into());
    names
}

/// A raw value: the common scalar types directly, the rest through a parsed value.
fn raw_value(v: RawBsonRef<'_>) -> Value {
    match v {
        RawBsonRef::Null => Value::Null,
        RawBsonRef::Boolean(b) => Value::Bool(b),
        RawBsonRef::Int32(n) => Value::I64(i64::from(n)),
        RawBsonRef::Int64(n) => Value::I64(n),
        RawBsonRef::Double(f) => Value::F64(f),
        RawBsonRef::String(s) => Value::Text(s.to_owned()),
        RawBsonRef::ObjectId(id) => Value::ObjectId(id.bytes()),
        other => Bson::try_from(other.to_raw_bson()).map_or(Value::Null, convert),
    }
}

/// One raw document as a row, without building a parsed document first: results of millions
/// of documents stream through here.
pub(crate) fn raw_row(names: &[String], doc: &RawDocument) -> Vec<Value> {
    let fields = names.len().saturating_sub(1);
    let mut out = vec![Value::Null; names.len()];
    let mut extra = Vec::new();
    for (k, v) in doc.iter().flatten() {
        let value = raw_value(v);
        match names[..fields].iter().position(|n| n == k) {
            Some(i) => out[i] = value,
            None => extra.push((k.to_owned(), value)),
        }
    }
    if !extra.is_empty()
        && let Some(last) = out.last_mut()
    {
        *last = Value::Document(extra);
    }
    out
}

/// A value as BSON, for edits.
pub(crate) fn to_bson(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::I64(n) => match i32::try_from(*n) {
            Ok(small) => Bson::Int32(small),
            Err(_) => Bson::Int64(*n),
        },
        Value::U64(n) => {
            i64::try_from(*n).map_or_else(|_| Bson::String(n.to_string()), Bson::Int64)
        }
        Value::F32(f) => Bson::Double(f64::from(*f)),
        Value::F64(f) => Bson::Double(*f),
        Value::Text(s) | Value::Decimal(s) => Bson::String(s.clone()),
        Value::ObjectId(b) => Bson::ObjectId(mongodb::bson::oid::ObjectId::from_bytes(*b)),
        Value::Uuid(u) => Bson::Binary(mongodb::bson::Binary {
            subtype: BinarySubtype::Uuid,
            bytes: u.to_vec(),
        }),
        Value::Bytes(b) => Bson::Binary(mongodb::bson::Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.bytes.clone(),
        }),
        Value::TimestampTz(tz) => Bson::DateTime(mongodb::bson::DateTime::from_millis(
            tz.utc.secs * 1_000 + i64::from(tz.utc.nanos / 1_000_000),
        )),
        Value::Timestamp(ts) => Bson::DateTime(mongodb::bson::DateTime::from_millis(
            ts.secs * 1_000 + i64::from(ts.nanos / 1_000_000),
        )),
        Value::Decimal128(d) => Bson::Decimal128(mongodb::bson::Decimal128::from_bytes(d.bytes)),
        Value::Json(j) => serde_json::from_str::<serde_json::Value>(&j.text)
            .ok()
            .and_then(|v| Bson::try_from(v).ok())
            .unwrap_or_else(|| Bson::String(j.text.clone())),
        Value::List(l) => Bson::Array(l.items.iter().map(to_bson).collect()),
        Value::Document(fields) => Bson::Document(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), to_bson(v)))
                .collect(),
        ),
        other => Bson::String(treasury_core::format::full_text(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn extended_json_parses() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"find": "o", "filter": {"_id": {"$oid": "65f0c0ffee0000000000abcd"}, "at": {"$date": "2024-02-29T13:45:30Z"}}}"#,
        )
        .unwrap();
        let d = document(json.as_object().unwrap().clone()).unwrap();
        let filter = d.get_document("filter").unwrap();
        assert!(matches!(filter.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(filter.get("at"), Some(Bson::DateTime(_))));
    }

    #[test]
    fn rows_keep_every_field() {
        let first = vec![doc! {"_id": 1, "a": "x"}];
        let names = columns(&first);
        assert_eq!(names, ["_id", "a", OTHER_FIELDS]);
        let r = row(&names, doc! {"b": true, "_id": 2});
        assert_eq!(r[0], Value::I64(2));
        assert_eq!(r[1], Value::Null);
        assert_eq!(r[2], Value::Document(vec![("b".into(), Value::Bool(true))]));
    }

    #[test]
    fn values_round_trip_through_bson() {
        for v in [
            Value::I64(5),
            Value::I64(1 << 40),
            Value::Text("é".into()),
            Value::ObjectId([7; 12]),
            Value::Uuid([9; 16]),
            Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: 1_709_214_330,
                    nanos: 120_000_000,
                },
                offset_secs: 0,
            }),
        ] {
            assert_eq!(convert(to_bson(&v)), v);
        }
    }
}
