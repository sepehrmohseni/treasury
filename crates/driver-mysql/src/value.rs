//! MySQL columns and values as Treasury's, both ways.

use mysql_async::Value as My;
use mysql_async::consts::{ColumnFlags, ColumnType};
use treasury_core::exec::{Column, ColumnSource, ValueKind};
use treasury_core::format::{format_uuid, full_text, hex};
use treasury_core::value::{
    Blob, Date, Json, Other, Time, Timestamp, TimestampTz, Value, civil_from_days, days_from_civil,
};

/// The binary character set: values in it are bytes, not text.
const BINARY_CHARSET: u16 = 63;

/// Text longer than this is cut, and binary values keep only their start.
const MAX_INLINE_BYTES: usize = 1024 * 1024;

/// How a column's values are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Signed,
    Unsigned,
    Float,
    Double,
    Decimal,
    Date,
    DateTime,
    Timestamp,
    Time,
    Year,
    Json,
    Bit,
    Binary,
    Text,
    Geometry,
    Other,
}

pub(crate) fn shape(col: &mysql_async::Column) -> Shape {
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = col.character_set() == BINARY_CHARSET;
    match col.column_type() {
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG => {
            if unsigned {
                Shape::Unsigned
            } else {
                Shape::Signed
            }
        }
        ColumnType::MYSQL_TYPE_FLOAT => Shape::Float,
        ColumnType::MYSQL_TYPE_DOUBLE => Shape::Double,
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => Shape::Decimal,
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => Shape::Date,
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => Shape::DateTime,
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => Shape::Timestamp,
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => Shape::Time,
        ColumnType::MYSQL_TYPE_YEAR => Shape::Year,
        ColumnType::MYSQL_TYPE_JSON => Shape::Json,
        ColumnType::MYSQL_TYPE_BIT => Shape::Bit,
        ColumnType::MYSQL_TYPE_GEOMETRY => Shape::Geometry,
        ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_ENUM
        | ColumnType::MYSQL_TYPE_SET
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if binary {
                Shape::Binary
            } else {
                Shape::Text
            }
        }
        _ => Shape::Other,
    }
}

fn type_name(col: &mysql_async::Column) -> &'static str {
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = col.character_set() == BINARY_CHARSET;
    let name = match col.column_type() {
        ColumnType::MYSQL_TYPE_TINY => "tinyint",
        ColumnType::MYSQL_TYPE_SHORT => "smallint",
        ColumnType::MYSQL_TYPE_INT24 => "mediumint",
        ColumnType::MYSQL_TYPE_LONG => "int",
        ColumnType::MYSQL_TYPE_LONGLONG => "bigint",
        ColumnType::MYSQL_TYPE_FLOAT => "float",
        ColumnType::MYSQL_TYPE_DOUBLE => "double",
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => "decimal",
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date",
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "datetime",
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "time",
        ColumnType::MYSQL_TYPE_YEAR => "year",
        ColumnType::MYSQL_TYPE_JSON => "json",
        ColumnType::MYSQL_TYPE_BIT => "bit",
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry",
        ColumnType::MYSQL_TYPE_ENUM => "enum",
        ColumnType::MYSQL_TYPE_SET => "set",
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        ColumnType::MYSQL_TYPE_STRING => {
            if col.flags().contains(ColumnFlags::ENUM_FLAG) {
                "enum"
            } else if col.flags().contains(ColumnFlags::SET_FLAG) {
                "set"
            } else if binary {
                "binary"
            } else {
                "char"
            }
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        _ => "unknown",
    };
    match (unsigned, name) {
        (true, "tinyint") => "tinyint unsigned",
        (true, "smallint") => "smallint unsigned",
        (true, "mediumint") => "mediumint unsigned",
        (true, "int") => "int unsigned",
        (true, "bigint") => "bigint unsigned",
        _ => name,
    }
}

pub(crate) fn kind(shape: Shape) -> ValueKind {
    match shape {
        Shape::Signed | Shape::Unsigned | Shape::Year => ValueKind::Integer,
        Shape::Float | Shape::Double => ValueKind::Float,
        Shape::Decimal => ValueKind::Decimal,
        Shape::Date => ValueKind::Date,
        Shape::DateTime => ValueKind::Timestamp,
        Shape::Timestamp => ValueKind::TimestampTz,
        Shape::Time => ValueKind::Time,
        Shape::Json => ValueKind::Json,
        Shape::Bit | Shape::Binary => ValueKind::Binary,
        Shape::Text => ValueKind::Text,
        Shape::Geometry | Shape::Other => ValueKind::Other,
    }
}

/// A result column, with the table column it came from when the server says.
pub(crate) fn column(col: &mysql_async::Column) -> Column {
    let mut c = Column::new(col.name_str(), type_name(col), kind(shape(col)));
    c.nullable = Some(!col.flags().contains(ColumnFlags::NOT_NULL_FLAG));
    let table = col.org_table_str();
    let name = col.org_name_str();
    if !table.is_empty() && !name.is_empty() {
        let schema = col.schema_str();
        c.source = Some(ColumnSource {
            schema: (!schema.is_empty()).then(|| schema.into_owned()),
            table: table.into_owned(),
            column: name.into_owned(),
        });
    }
    c
}

fn date(y: u16, m: u8, d: u8) -> Option<i32> {
    if y == 0 || m == 0 || d == 0 {
        return None;
    }
    i32::try_from(days_from_civil(i64::from(y), u32::from(m), u32::from(d))).ok()
}

fn other(type_name: &str, text: String) -> Value {
    Value::Other(Other {
        type_name: type_name.to_owned(),
        text,
    })
}

fn timestamp(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> Option<Timestamp> {
    let days = i64::from(date(y, mo, d)?);
    Some(Timestamp {
        secs: days * 86_400 + i64::from(h) * 3_600 + i64::from(mi) * 60 + i64::from(s),
        nanos: us * 1_000,
    })
}

fn time(neg: bool, days: u32, h: u8, m: u8, s: u8, us: u32) -> Value {
    let secs = u64::from(days) * 86_400 + u64::from(h) * 3_600 + u64::from(m) * 60 + u64::from(s);
    if !neg && secs < 86_400 {
        return Value::Time(Time {
            nanos: secs * 1_000_000_000 + u64::from(us) * 1_000,
        });
    }
    // TIME is a duration in MySQL and may exceed a day or be negative.
    let hours = u64::from(days) * 24 + u64::from(h);
    let mut text = format!("{}{hours:02}:{m:02}:{s:02}", if neg { "-" } else { "" });
    if us > 0 {
        text.push_str(&format!(".{us:06}"));
    }
    other("time", text)
}

fn text_or_bytes(bytes: Vec<u8>, binary: bool) -> Value {
    if binary {
        return blob(bytes);
    }
    match String::from_utf8(bytes) {
        Ok(mut s) => {
            if s.len() > MAX_INLINE_BYTES {
                let mut cut = MAX_INLINE_BYTES;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
            }
            Value::Text(s)
        }
        Err(e) => blob(e.into_bytes()),
    }
}

fn blob(mut bytes: Vec<u8>) -> Value {
    let total = u64::try_from(bytes.len()).ok();
    let complete = bytes.len() <= MAX_INLINE_BYTES;
    bytes.truncate(MAX_INLINE_BYTES);
    Value::Bytes(Blob {
        bytes,
        total_len: total,
        complete,
    })
}

/// Parses `YYYY-MM-DD[ HH:MM:SS[.ffffff]]`, the text protocol's form.
fn parse_datetime(t: &str) -> Option<(u16, u8, u8, u8, u8, u8, u32)> {
    let (date_part, time_part) = t.split_once(' ').unwrap_or((t, "00:00:00"));
    let mut d = date_part.splitn(3, '-');
    let y = d.next()?.parse().ok()?;
    let mo = d.next()?.parse().ok()?;
    let day = d.next()?.parse().ok()?;
    let (hms, frac) = time_part.split_once('.').unwrap_or((time_part, ""));
    let mut p = hms.splitn(3, ':');
    let h = p.next()?.parse().ok()?;
    let mi = p.next()?.parse().ok()?;
    let s = p.next()?.parse().ok()?;
    let us = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").get(..6)?.parse().ok()?
    };
    Some((y, mo, day, h, mi, s, us))
}

/// Parses `[-]HHH:MM:SS[.ffffff]`.
fn parse_time(t: &str) -> Option<Value> {
    let (neg, rest) = t.strip_prefix('-').map_or((false, t), |r| (true, r));
    let (hms, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let mut p = hms.splitn(3, ':');
    let hours: u32 = p.next()?.parse().ok()?;
    let m = p.next()?.parse().ok()?;
    let s = p.next()?.parse().ok()?;
    let us = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").get(..6)?.parse().ok()?
    };
    Some(time(
        neg,
        hours / 24,
        u8::try_from(hours % 24).ok()?,
        m,
        s,
        us,
    ))
}

/// One value of a column of `shape`, from either protocol.
pub(crate) fn convert(shape: Shape, json_binary: bool, v: My) -> Value {
    match (shape, v) {
        (_, My::NULL) => Value::Null,
        (_, My::Int(n)) => Value::I64(n),
        (_, My::UInt(n)) => Value::U64(n),
        (_, My::Float(f)) => Value::F32(f),
        (_, My::Double(f)) => Value::F64(f),
        (s, My::Date(y, mo, d, h, mi, sec, us)) => match s {
            Shape::Date => match date(y, mo, d) {
                Some(days) => Value::Date(Date { days }),
                None => other("date", format!("{y:04}-{mo:02}-{d:02}")),
            },
            Shape::Timestamp => match timestamp(y, mo, d, h, mi, sec, us) {
                // The session's time zone is UTC, so TIMESTAMP values read as UTC instants.
                Some(utc) => Value::TimestampTz(TimestampTz {
                    utc,
                    offset_secs: 0,
                }),
                None => other(
                    "timestamp",
                    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{sec:02}"),
                ),
            },
            _ => match timestamp(y, mo, d, h, mi, sec, us) {
                Some(ts) => Value::Timestamp(ts),
                None => other(
                    "datetime",
                    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{sec:02}"),
                ),
            },
        },
        (_, My::Time(neg, days, h, m, s, us)) => time(neg, days, h, m, s, us),
        (s, My::Bytes(b)) => from_bytes(s, json_binary, b),
    }
}

/// A value the text protocol (or the binary protocol, for strings) sent as bytes.
fn from_bytes(shape: Shape, json_binary: bool, b: Vec<u8>) -> Value {
    let text = || String::from_utf8_lossy(&b).into_owned();
    match shape {
        Shape::Signed | Shape::Year => text()
            .parse()
            .map_or_else(|_| other("integer", text()), Value::I64),
        Shape::Unsigned => text()
            .parse()
            .map_or_else(|_| other("integer", text()), Value::U64),
        Shape::Float => text()
            .parse()
            .map_or_else(|_| other("float", text()), Value::F32),
        Shape::Double => text()
            .parse()
            .map_or_else(|_| other("double", text()), Value::F64),
        Shape::Decimal => Value::Decimal(text()),
        Shape::Date | Shape::DateTime | Shape::Timestamp => match parse_datetime(&text()) {
            Some((y, mo, d, h, mi, s, us)) => {
                convert(shape, json_binary, My::Date(y, mo, d, h, mi, s, us))
            }
            None => other("datetime", text()),
        },
        Shape::Time => parse_time(&text()).unwrap_or_else(|| other("time", text())),
        Shape::Json => Value::Json(Json {
            text: text(),
            binary: json_binary,
        }),
        Shape::Bit | Shape::Binary => blob(b),
        Shape::Text => text_or_bytes(b, false),
        Shape::Geometry => other("geometry", format!("0x{}", hex(&b))),
        Shape::Other => text_or_bytes(b, false),
    }
}

/// A bind parameter value.
pub(crate) fn to_mysql(v: &Value) -> My {
    match v {
        Value::Null => My::NULL,
        Value::Bool(b) => My::Int(i64::from(*b)),
        Value::I64(n) => My::Int(*n),
        Value::U64(n) => My::UInt(*n),
        Value::F32(f) => My::Float(*f),
        Value::F64(f) => My::Double(*f),
        Value::Text(s) | Value::Decimal(s) => My::Bytes(s.clone().into_bytes()),
        Value::Bytes(b) => My::Bytes(b.bytes.clone()),
        Value::Json(j) => My::Bytes(j.text.clone().into_bytes()),
        Value::Uuid(u) => My::Bytes(format_uuid(u).into_bytes()),
        Value::Date(d) => {
            let (y, m, day) = civil_from_days(i64::from(d.days));
            match (u16::try_from(y), u8::try_from(m), u8::try_from(day)) {
                (Ok(y), Ok(m), Ok(day)) => My::Date(y, m, day, 0, 0, 0, 0),
                _ => My::Bytes(full_text(v).into_bytes()),
            }
        }
        Value::Timestamp(ts) | Value::TimestampTz(TimestampTz { utc: ts, .. }) => {
            let days = ts.secs.div_euclid(86_400);
            let secs = ts.secs.rem_euclid(86_400);
            let (y, m, d) = civil_from_days(days);
            match (u16::try_from(y), u8::try_from(m), u8::try_from(d)) {
                (Ok(y), Ok(m), Ok(d)) => My::Date(
                    y,
                    m,
                    d,
                    u8::try_from(secs / 3_600).unwrap_or(0),
                    u8::try_from(secs % 3_600 / 60).unwrap_or(0),
                    u8::try_from(secs % 60).unwrap_or(0),
                    ts.nanos / 1_000,
                ),
                _ => My::Bytes(full_text(v).into_bytes()),
            }
        }
        Value::Time(t) => {
            let secs = t.nanos / 1_000_000_000;
            My::Time(
                false,
                0,
                u8::try_from(secs / 3_600).unwrap_or(0),
                u8::try_from(secs % 3_600 / 60).unwrap_or(0),
                u8::try_from(secs % 60).unwrap_or(0),
                u32::try_from(t.nanos % 1_000_000_000 / 1_000).unwrap_or(0),
            )
        }
        other => My::Bytes(full_text(other).into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_values() {
        assert_eq!(convert(Shape::Signed, false, My::Int(-3)), Value::I64(-3));
        assert_eq!(
            convert(Shape::Date, false, My::Date(2024, 2, 29, 0, 0, 0, 0)),
            Value::Date(Date { days: 19_782 })
        );
        assert!(matches!(
            convert(Shape::Date, false, My::Date(0, 0, 0, 0, 0, 0, 0)),
            Value::Other(_)
        ));
        assert_eq!(
            convert(
                Shape::DateTime,
                false,
                My::Date(1970, 1, 2, 0, 0, 1, 500_000)
            ),
            Value::Timestamp(Timestamp {
                secs: 86_401,
                nanos: 500_000_000
            })
        );
        assert_eq!(
            convert(Shape::Time, false, My::Time(false, 0, 1, 2, 3, 0)),
            Value::Time(Time {
                nanos: 3_723_000_000_000
            })
        );
        assert_eq!(
            convert(Shape::Time, false, My::Time(true, 1, 2, 0, 0, 0)),
            other("time", "-26:00:00".into())
        );
    }

    #[test]
    fn text_values_parse_like_binary_ones() {
        for (shape, text, binary) in [
            (Shape::Signed, "-3", My::Int(-3)),
            (Shape::Unsigned, "18446744073709551615", My::UInt(u64::MAX)),
            (
                Shape::DateTime,
                "2024-02-29 13:45:30.25",
                My::Date(2024, 2, 29, 13, 45, 30, 250_000),
            ),
            (Shape::Date, "2024-02-29", My::Date(2024, 2, 29, 0, 0, 0, 0)),
            (Shape::Time, "838:59:59", My::Time(false, 34, 22, 59, 59, 0)),
            (
                Shape::Time,
                "13:45:30.5",
                My::Time(false, 0, 13, 45, 30, 500_000),
            ),
        ] {
            assert_eq!(
                convert(shape, false, My::Bytes(text.as_bytes().to_vec())),
                convert(shape, false, binary),
                "{text}"
            );
        }
        assert_eq!(
            convert(Shape::Decimal, false, My::Bytes(b"-12.3400".to_vec())),
            Value::Decimal("-12.3400".into())
        );
        assert!(matches!(
            convert(Shape::Text, false, My::Bytes(vec![0xff, 0xfe])),
            Value::Bytes(_)
        ));
    }

    #[test]
    fn parameters_round_trip_through_conversion() {
        for v in [
            Value::Date(Date { days: 19_782 }),
            Value::Timestamp(Timestamp {
                secs: 1_709_214_330,
                nanos: 123_456_000,
            }),
            Value::Time(Time {
                nanos: 49_530_500_000_000,
            }),
        ] {
            let shape = match v {
                Value::Date(_) => Shape::Date,
                Value::Time(_) => Shape::Time,
                _ => Shape::DateTime,
            };
            assert_eq!(convert(shape, false, to_mysql(&v)), v);
        }
    }
}
