//! Values as JSON, for JSON and NDJSON exports and for copying rows as JSON.
//!
//! Numbers that JSON consumers would round (decimals, integers beyond 2^53 are kept exact as
//! numbers only up to 64 bits) stay exact: decimals and 128-bit integers become strings. Binary
//! becomes base64. Temporal values use ISO 8601. MongoDB types use Extended JSON (canonical),
//! so the output can be imported back.

use crate::format::{format_date, format_interval, format_offset, format_time, format_uuid, hex};
use crate::value::{BsonSpecial, ListKind, Timestamp, Value};
use serde_json::{Map, Number, Value as J, json};

/// The JSON form of `v`.
pub fn to_json(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::I64(n) => J::from(*n),
        Value::U64(n) => J::from(*n),
        Value::I128(n) => i64::try_from(*n).map_or_else(|_| J::String(n.to_string()), J::from),
        Value::U128(n) => u64::try_from(*n).map_or_else(|_| J::String(n.to_string()), J::from),
        Value::F32(f) => float(f64::from(*f)),
        Value::F64(f) => float(*f),
        Value::Decimal(s) => J::String(s.clone()),
        Value::Text(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(base64(&b.bytes)),
        Value::Date(d) => J::String(format_date(*d)),
        Value::Time(t) => J::String(format_time(*t)),
        Value::TimeTz(t) => J::String(format!(
            "{}{}",
            format_time(t.time),
            format_offset(t.offset_secs)
        )),
        Value::Timestamp(ts) => J::String(iso_timestamp(*ts)),
        Value::TimestampTz(tz) => {
            let local = Timestamp {
                secs: tz.utc.secs.saturating_add(i64::from(tz.offset_secs)),
                nanos: tz.utc.nanos,
            };
            J::String(format!(
                "{}{}",
                iso_timestamp(local),
                format_offset(tz.offset_secs)
            ))
        }
        Value::Infinite(i) => J::String(if i.negative { "-infinity" } else { "infinity" }.into()),
        Value::Interval(iv) => J::String(format_interval(*iv)),
        Value::Uuid(u) => J::String(format_uuid(u)),
        Value::Json(j) => {
            serde_json::from_str(&j.text).unwrap_or_else(|_| J::String(j.text.clone()))
        }
        Value::List(l) => match l.kind {
            ListKind::Array | ListKind::List | ListKind::Set | ListKind::Tuple => {
                J::Array(l.items.iter().map(to_json).collect())
            }
        },
        Value::Composite(c) => J::Object(
            c.fields
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect(),
        ),
        Value::Map(m) => {
            if m.iter().all(|(k, _)| matches!(k, Value::Text(_))) {
                J::Object(
                    m.iter()
                        .map(|(k, v)| match k {
                            Value::Text(s) => (s.clone(), to_json(v)),
                            _ => (String::new(), J::Null),
                        })
                        .collect(),
                )
            } else {
                J::Array(
                    m.iter()
                        .map(|(k, v)| json!([to_json(k), to_json(v)]))
                        .collect(),
                )
            }
        }
        Value::Document(d) => J::Object(
            d.iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect::<Map<_, _>>(),
        ),
        Value::ObjectId(id) => json!({ "$oid": hex(id) }),
        Value::Decimal128(d) => json!({ "$numberDecimal": d.text }),
        Value::Bson(b) => match b {
            BsonSpecial::Regex { pattern, options } => {
                json!({ "$regularExpression": { "pattern": pattern, "options": options } })
            }
            BsonSpecial::JavaScript(code) => json!({ "$code": code }),
            BsonSpecial::Timestamp { time, increment } => {
                json!({ "$timestamp": { "t": time, "i": increment } })
            }
            BsonSpecial::Binary { subtype, bytes } => {
                json!({ "$binary": { "base64": base64(bytes), "subType": format!("{subtype:02x}") } })
            }
            BsonSpecial::Undefined => json!({ "$undefined": true }),
            BsonSpecial::MinKey => json!({ "$minKey": 1 }),
            BsonSpecial::MaxKey => json!({ "$maxKey": 1 }),
            BsonSpecial::Symbol(s) => json!({ "$symbol": s }),
        },
        Value::Other(o) => J::String(o.text.clone()),
    }
}

fn float(f: f64) -> J {
    match Number::from_f64(f) {
        Some(n) => J::Number(n),
        None if f.is_nan() => J::String("NaN".into()),
        None if f > 0.0 => J::String("Infinity".into()),
        None => J::String("-Infinity".into()),
    }
}

/// `2024-02-29T13:05:09.123`
fn iso_timestamp(ts: Timestamp) -> String {
    crate::format::format_timestamp(ts).replacen(' ', "T", 1)
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let idx = |shift: u32| char::from(T[usize::try_from((n >> shift) & 63).unwrap_or(0)]);
        out.push(idx(18));
        out.push(idx(12));
        out.push(if chunk.len() > 1 { idx(6) } else { '=' });
        out.push(if chunk.len() > 2 { idx(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::*;

    #[test]
    fn exactness_is_kept() {
        assert_eq!(
            to_json(&Value::Decimal("12345678901234567890.12".into())),
            json!("12345678901234567890.12")
        );
        assert_eq!(
            to_json(&Value::I128(i128::MAX)),
            json!(i128::MAX.to_string())
        );
        assert_eq!(to_json(&Value::I128(5)), json!(5));
        assert_eq!(to_json(&Value::U64(u64::MAX)), json!(u64::MAX));
        assert_eq!(to_json(&Value::F64(f64::NAN)), json!("NaN"));
    }

    #[test]
    fn iso_temporal_values() {
        assert_eq!(
            to_json(&Value::Timestamp(Timestamp {
                secs: 1_709_211_909,
                nanos: 5_000_000
            })),
            json!("2024-02-29T13:05:09.005")
        );
        assert_eq!(
            to_json(&Value::TimestampTz(TimestampTz {
                utc: Timestamp { secs: 0, nanos: 0 },
                offset_secs: -3600
            })),
            json!("1969-12-31T23:00:00-01:00")
        );
    }

    #[test]
    fn nested_and_extended_json() {
        let doc = Value::Document(vec![
            ("_id".into(), Value::ObjectId([1; 12])),
            (
                "tags".into(),
                Value::List(List {
                    kind: ListKind::Array,
                    items: vec![Value::Text("a".into())],
                }),
            ),
            (
                "raw".into(),
                Value::Json(Json {
                    text: "{\"x\":[1,2]}".into(),
                    binary: true,
                }),
            ),
        ]);
        assert_eq!(
            to_json(&doc),
            json!({ "_id": { "$oid": "010101010101010101010101" }, "tags": ["a"], "raw": { "x": [1, 2] } })
        );
        let m = Value::Map(vec![(Value::I64(1), Value::Text("one".into()))]);
        assert_eq!(to_json(&m), json!([[1, "one"]]));
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
