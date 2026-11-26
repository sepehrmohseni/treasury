//! PostgreSQL's text output format turned into values. The session sets `DateStyle=ISO`,
//! `IntervalStyle=iso_8601`, `TimeZone=UTC`, `bytea_output=hex` and `extra_float_digits=1` at
//! connect, so every type has one predictable spelling. Types without a structured form (ranges,
//! network addresses, geometry, extension types) keep their text exactly.

use postgres_types::{Kind, Type};
use treasury_core::value::{
    Blob, Composite, Date, Infinite, Interval, Json, List, ListKind, Other, TemporalKind, Time,
    TimeTz, Timestamp, TimestampTz, Value, days_from_civil,
};

/// The largest binary value kept whole; longer ones keep their start and their length.
pub const MAX_INLINE_BYTES: usize = 1024 * 1024;
/// The largest text value accepted from the server.
pub const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// Parses `text` of column type `ty`. Malformed text falls back to [`Value::Other`] with the
/// original spelling, so nothing is ever lost or invented.
pub fn parse(ty: &Type, text: &str) -> Value {
    if text.len() > MAX_TEXT_BYTES {
        return Value::Other(Other {
            type_name: ty.name().to_owned(),
            text: format!(
                "(a {} MB value, larger than Treasury displays)",
                text.len() / 1_000_000
            ),
        });
    }
    parse_known(ty, text).unwrap_or_else(|| other(ty, text))
}

fn other(ty: &Type, text: &str) -> Value {
    Value::Other(Other {
        type_name: ty.name().to_owned(),
        text: text.to_owned(),
    })
}

fn parse_known(ty: &Type, text: &str) -> Option<Value> {
    match ty.kind() {
        Kind::Array(elem) => return parse_array(elem, text),
        Kind::Domain(base) => return parse_known(base, text),
        Kind::Enum(_) => return Some(Value::Text(text.to_owned())),
        Kind::Composite(fields) => {
            let parts = split_composite(text)?;
            if parts.len() != fields.len() {
                return None;
            }
            return Some(Value::Composite(Composite {
                type_name: Some(ty.name().to_owned()),
                fields: fields
                    .iter()
                    .zip(parts)
                    .map(|(f, p)| {
                        (
                            f.name().to_owned(),
                            p.map_or(Value::Null, |t| parse(f.type_(), &t)),
                        )
                    })
                    .collect(),
            }));
        }
        _ => {}
    }
    Some(match *ty {
        Type::BOOL => match text {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => return None,
        },
        Type::INT2
        | Type::INT4
        | Type::INT8
        | Type::OID
        | Type::XID
        | Type::CID
        | Type::REGPROC => Value::I64(text.parse().ok()?),
        Type::FLOAT4 => Value::F32(parse_float(text)? as f32),
        Type::FLOAT8 => Value::F64(parse_float(text)?),
        Type::NUMERIC => Value::Decimal(text.to_owned()),
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::CHAR
        | Type::UNKNOWN
        | Type::XML => Value::Text(text.to_owned()),
        Type::BYTEA => Value::Bytes(parse_bytea(text)?),
        Type::UUID => Value::Uuid(parse_uuid(text)?),
        Type::JSON => Value::Json(Json {
            text: text.to_owned(),
            binary: false,
        }),
        Type::JSONB => Value::Json(Json {
            text: text.to_owned(),
            binary: true,
        }),
        Type::DATE => match text {
            "infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::Date,
                negative: false,
            }),
            "-infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::Date,
                negative: true,
            }),
            _ => Value::Date(Date {
                days: i32::try_from(parse_date(text)?).ok()?,
            }),
        },
        Type::TIME => Value::Time(Time {
            nanos: parse_time(text)?,
        }),
        Type::TIMETZ => {
            let (t, off) = split_offset(text)?;
            Value::TimeTz(TimeTz {
                time: Time {
                    nanos: parse_time(t)?,
                },
                offset_secs: off,
            })
        }
        Type::TIMESTAMP => match text {
            "infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::Timestamp,
                negative: false,
            }),
            "-infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::Timestamp,
                negative: true,
            }),
            _ => Value::Timestamp(parse_timestamp(text)?),
        },
        Type::TIMESTAMPTZ => match text {
            "infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::TimestampTz,
                negative: false,
            }),
            "-infinity" => Value::Infinite(Infinite {
                kind: TemporalKind::TimestampTz,
                negative: true,
            }),
            _ => {
                let (body, era_bc) = strip_bc(text);
                let (local, off) = split_offset(body)?;
                let ts = parse_timestamp(&if era_bc {
                    format!("{local} BC")
                } else {
                    local.to_owned()
                })?;
                Value::TimestampTz(TimestampTz {
                    utc: Timestamp {
                        secs: ts.secs.checked_sub(i64::from(off))?,
                        nanos: ts.nanos,
                    },
                    offset_secs: off,
                })
            }
        },
        Type::INTERVAL => Value::Interval(parse_iso_interval(text)?),
        _ => return None,
    })
}

fn parse_float(text: &str) -> Option<f64> {
    match text {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        t => t.parse().ok(),
    }
}

fn parse_bytea(text: &str) -> Option<Blob> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let total = hex.len() / 2;
    let keep = total.min(MAX_INLINE_BYTES);
    let mut bytes = Vec::with_capacity(keep);
    for i in 0..keep {
        bytes.push(u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?);
    }
    Some(Blob {
        bytes,
        total_len: u64::try_from(total).ok(),
        complete: keep == total,
    })
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn strip_bc(text: &str) -> (&str, bool) {
    match text.strip_suffix(" BC") {
        Some(t) => (t, true),
        None => (text, false),
    }
}

/// `2024-02-29`, `0044-03-15 BC` (year 44 BC is astronomical year -43).
fn parse_date(text: &str) -> Option<i64> {
    let (body, bc) = strip_bc(text);
    let mut it = body.splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let year = if bc { 1 - y } else { y };
    Some(days_from_civil(year, m, d))
}

/// `13:05:09`, `13:05:09.250001`, `24:00:00`.
fn parse_time(text: &str) -> Option<u64> {
    let (hms, frac) = match text.split_once('.') {
        Some((a, b)) => (a, b),
        None => (text, ""),
    };
    let mut it = hms.splitn(3, ':');
    let h: u64 = it.next()?.parse().ok()?;
    let m: u64 = it.next()?.parse().ok()?;
    let s: u64 = it.next()?.parse().ok()?;
    if h > 24 || m > 59 || s > 60 || frac.len() > 9 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let nanos = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<9}").parse::<u64>().ok()?
    };
    Some(((h * 60 + m) * 60 + s) * 1_000_000_000 + nanos)
}

/// Splits `HH:MM:SS+05:30` or `...-08` into the local part and the offset in seconds.
fn split_offset(text: &str) -> Option<(&str, i32)> {
    let idx = text.rfind(['+', '-']).filter(|&i| i >= 8)?;
    let (local, off) = text.split_at(idx);
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let mut parts = off[1..].split(':');
    let h: i32 = parts.next()?.parse().ok()?;
    let m: i32 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let s: i32 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((local, sign * (h * 3600 + m * 60 + s)))
}

/// `2024-02-29 13:05:09.123`, with an optional ` BC`.
fn parse_timestamp(text: &str) -> Option<Timestamp> {
    let (body, bc) = strip_bc(text);
    let (date, time) = body.split_once(' ')?;
    let days = parse_date(&if bc {
        format!("{date} BC")
    } else {
        date.to_owned()
    })?;
    let nanos = parse_time(time)?;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::try_from(nanos / 1_000_000_000).ok()?)?;
    Some(Timestamp {
        secs,
        nanos: u32::try_from(nanos % 1_000_000_000).ok()?,
    })
}

/// ISO 8601 durations as PostgreSQL writes them: `P1Y2M3DT4H5M6.5S`, `PT-1H`, `P-1D`.
fn parse_iso_interval(text: &str) -> Option<Interval> {
    let body = text.strip_prefix('P')?;
    let (date, time) = match body.split_once('T') {
        Some((d, t)) => (d, t),
        None => (body, ""),
    };
    let mut months: i64 = 0;
    let mut days: i64 = 0;
    let mut nanos: i64 = 0;
    for (num, unit) in units(date)? {
        let n: i64 = num.parse().ok()?;
        match unit {
            'Y' => months += n * 12,
            'M' => months += n,
            'W' => days += n * 7,
            'D' => days += n,
            _ => return None,
        }
    }
    for (num, unit) in units(time)? {
        match unit {
            'H' => nanos += num.parse::<i64>().ok()? * 3_600_000_000_000,
            'M' => nanos += num.parse::<i64>().ok()? * 60_000_000_000,
            'S' => {
                let neg = num.starts_with('-');
                let n = num.trim_start_matches('-');
                let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
                let w: i64 = whole.parse().ok()?;
                let f: i64 = if frac.is_empty() {
                    0
                } else {
                    format!("{frac:0<9}").get(..9)?.parse().ok()?
                };
                let v = w * 1_000_000_000 + f;
                nanos += if neg { -v } else { v };
            }
            _ => return None,
        }
    }
    Some(Interval {
        months: i32::try_from(months).ok()?,
        days: i32::try_from(days).ok()?,
        nanos,
    })
}

fn units(s: &str) -> Option<Vec<(&str, char)>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_alphabetic() {
            out.push((s.get(start..i)?, c));
            start = i + 1;
        }
    }
    (start == s.len()).then_some(out)
}

/// Array literals: `{1,2,NULL}`, `{"a b","c\"d"}`, `{{1,2},{3,4}}`, `[0:1]={1,2}`.
fn parse_array(elem: &Type, text: &str) -> Option<Value> {
    let body = match text.find("={") {
        Some(i) if text.starts_with('[') => &text[i + 1..],
        _ => text,
    };
    let mut chars = body.chars().peekable();
    let v = array_level(elem, &mut chars)?;
    chars.peek().is_none().then_some(v)
}

fn array_level(elem: &Type, it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Value> {
    if it.next()? != '{' {
        return None;
    }
    let mut items = Vec::new();
    if it.peek() == Some(&'}') {
        it.next();
        return Some(Value::List(List {
            kind: ListKind::Array,
            items,
        }));
    }
    loop {
        match it.peek()? {
            '{' => items.push(array_level(elem, it)?),
            '"' => {
                it.next();
                let mut s = String::new();
                loop {
                    match it.next()? {
                        '\\' => s.push(it.next()?),
                        '"' => break,
                        c => s.push(c),
                    }
                }
                items.push(parse(elem, &s));
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = it.peek() {
                    if c == ',' || c == '}' {
                        break;
                    }
                    s.push(c);
                    it.next();
                }
                items.push(if s == "NULL" {
                    Value::Null
                } else {
                    parse(elem, &s)
                });
            }
        }
        match it.next()? {
            ',' => continue,
            '}' => break,
            _ => return None,
        }
    }
    Some(Value::List(List {
        kind: ListKind::Array,
        items,
    }))
}

/// Composite literals: `(1,"a b",)` with an empty field meaning NULL.
fn split_composite(text: &str) -> Option<Vec<Option<String>>> {
    let inner = text.strip_prefix('(')?.strip_suffix(')')?;
    let mut out = Vec::new();
    let mut it = inner.chars().peekable();
    loop {
        if it.peek() == Some(&'"') {
            it.next();
            let mut s = String::new();
            loop {
                match it.next()? {
                    '\\' => s.push(it.next()?),
                    '"' if it.peek() == Some(&'"') => {
                        it.next();
                        s.push('"');
                    }
                    '"' => break,
                    c => s.push(c),
                }
            }
            out.push(Some(s));
        } else {
            let mut s = String::new();
            while let Some(&c) = it.peek() {
                if c == ',' {
                    break;
                }
                s.push(c);
                it.next();
            }
            out.push(if s.is_empty() { None } else { Some(s) });
        }
        match it.next() {
            Some(',') => continue,
            None => break,
            Some(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse(&Type::BOOL, "t"), Value::Bool(true));
        assert_eq!(
            parse(&Type::INT8, "-9223372036854775808"),
            Value::I64(i64::MIN)
        );
        assert_eq!(
            parse(&Type::NUMERIC, "12345678901234567890.0100"),
            Value::Decimal("12345678901234567890.0100".into())
        );
        assert_eq!(parse(&Type::FLOAT8, "0.1"), Value::F64(0.1));
        assert!(matches!(parse(&Type::FLOAT8, "NaN"), Value::F64(f) if f.is_nan()));
        assert_eq!(
            parse(&Type::BYTEA, "\\xdead"),
            Value::Bytes(Blob::full(vec![0xde, 0xad]))
        );
        assert_eq!(
            parse(&Type::UUID, "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            Value::Uuid([
                0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38,
                0x0a, 0x11
            ])
        );
        assert_eq!(
            parse(&Type::JSONB, "{\"a\": 1}"),
            Value::Json(Json {
                text: "{\"a\": 1}".into(),
                binary: true
            })
        );
    }

    #[test]
    fn temporal() {
        assert_eq!(
            parse(&Type::DATE, "2024-02-29"),
            Value::Date(Date { days: 19_782 })
        );
        assert_eq!(
            parse(&Type::DATE, "0044-03-15 BC"),
            Value::Date(Date {
                days: i32::try_from(days_from_civil(-43, 3, 15)).unwrap()
            })
        );
        assert_eq!(
            parse(&Type::TIME, "13:05:09.25"),
            Value::Time(Time {
                nanos: 47_109_250_000_000
            })
        );
        assert_eq!(
            parse(&Type::TIMESTAMP, "2024-02-29 13:05:09.123456"),
            Value::Timestamp(Timestamp {
                secs: 1_709_211_909,
                nanos: 123_456_000
            })
        );
        assert_eq!(
            parse(&Type::TIMESTAMPTZ, "2024-02-29 18:35:09+05:30"),
            Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: 1_709_211_909,
                    nanos: 0
                },
                offset_secs: 19_800
            })
        );
        assert_eq!(
            parse(&Type::TIMESTAMPTZ, "2024-02-29 13:05:09-08"),
            Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: 1_709_211_909 + 8 * 3600,
                    nanos: 0
                },
                offset_secs: -28_800
            })
        );
        assert_eq!(
            parse(&Type::TIMESTAMPTZ, "infinity"),
            Value::Infinite(Infinite {
                kind: TemporalKind::TimestampTz,
                negative: false
            })
        );
        assert_eq!(
            parse(&Type::TIMETZ, "13:05:09+02"),
            Value::TimeTz(TimeTz {
                time: Time {
                    nanos: 47_109_000_000_000
                },
                offset_secs: 7_200
            })
        );
    }

    #[test]
    fn intervals() {
        assert_eq!(
            parse(&Type::INTERVAL, "P1Y2M3DT4H5M6.5S"),
            Value::Interval(Interval {
                months: 14,
                days: 3,
                nanos: 4 * 3_600_000_000_000 + 5 * 60_000_000_000 + 6_500_000_000
            })
        );
        assert_eq!(
            parse(&Type::INTERVAL, "PT-1H"),
            Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos: -3_600_000_000_000
            })
        );
        assert_eq!(
            parse(&Type::INTERVAL, "PT0S"),
            Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos: 0
            })
        );
        assert_eq!(
            parse(&Type::INTERVAL, "PT-0.000001S"),
            Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos: -1_000
            })
        );
    }

    #[test]
    fn arrays() {
        let ints = Type::INT4_ARRAY;
        assert_eq!(
            parse(&ints, "{1,NULL,3}"),
            Value::List(List {
                kind: ListKind::Array,
                items: vec![Value::I64(1), Value::Null, Value::I64(3)]
            })
        );
        assert_eq!(
            parse(&Type::TEXT_ARRAY, "{\"a b\",\"c\\\"d\",plain,\"NULL\"}"),
            Value::List(List {
                kind: ListKind::Array,
                items: vec![
                    Value::Text("a b".into()),
                    Value::Text("c\"d".into()),
                    Value::Text("plain".into()),
                    Value::Text("NULL".into())
                ]
            })
        );
        let nested = parse(&ints, "{{1,2},{3,4}}");
        let Value::List(outer) = nested else { panic!() };
        assert_eq!(outer.items.len(), 2);
        assert_eq!(
            parse(&ints, "{}"),
            Value::List(List {
                kind: ListKind::Array,
                items: vec![]
            })
        );
        assert!(matches!(parse(&ints, "[0:1]={7,8}"), Value::List(l) if l.items.len() == 2));
    }

    #[test]
    fn unknown_and_malformed_text_is_kept() {
        assert_eq!(
            parse(&Type::INET, "10.0.0.1/8"),
            Value::Other(Other {
                type_name: "inet".into(),
                text: "10.0.0.1/8".into()
            })
        );
        assert_eq!(
            parse(&Type::BOOL, "maybe"),
            Value::Other(Other {
                type_name: "bool".into(),
                text: "maybe".into()
            })
        );
        assert_eq!(
            parse(&Type::INT4_ARRAY, "{1,2"),
            Value::Other(Other {
                type_name: "_int4".into(),
                text: "{1,2".into()
            })
        );
    }

    #[test]
    fn composites_split_quoted_fields() {
        assert_eq!(
            split_composite("(1,\"a, b\",,\"x\"\"y\")").unwrap(),
            vec![
                Some("1".into()),
                Some("a, b".into()),
                None,
                Some("x\"y".into())
            ]
        );
    }

    #[test]
    fn large_bytea_keeps_its_start() {
        let hex = "ab".repeat(MAX_INLINE_BYTES + 10);
        let Value::Bytes(b) = parse(&Type::BYTEA, &format!("\\x{hex}")) else {
            panic!()
        };
        assert!(!b.complete);
        assert_eq!(b.bytes.len(), MAX_INLINE_BYTES);
        assert_eq!(
            b.total_len,
            Some(u64::try_from(MAX_INLINE_BYTES + 10).unwrap())
        );
    }
}
