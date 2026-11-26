//! The binary format, used only for statements with bind parameters (tokio-postgres reads
//! results in binary then). Common types are decoded; others keep their bytes as hex with their
//! type name. Parameters are encoded for the type the server inferred.

use bytes::{BufMut, BytesMut};
use postgres_types::{Format, FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};
use std::error::Error as StdError;
use treasury_core::value::{
    Blob, Date, Infinite, Interval, Json, List, ListKind, Other, TemporalKind, Time, TimeTz,
    Timestamp, TimestampTz, Value,
};

type BoxError = Box<dyn StdError + Sync + Send>;

/// Seconds from 1970-01-01 to 2000-01-01, PostgreSQL's epoch.
const PG_EPOCH_SECS: i64 = 946_684_800;
const PG_EPOCH_DAYS: i32 = 10_957;

/// A decoded column value.
pub struct Decoded(pub Value);

impl<'a> FromSql<'a> for Decoded {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(Decoded(decode(ty, raw).unwrap_or_else(|| {
            Value::Other(Other {
                type_name: ty.name().to_owned(),
                text: format!("\\x{}", treasury_core::format::hex(raw)),
            })
        })))
    }

    fn from_sql_null(_: &Type) -> Result<Self, BoxError> {
        Ok(Decoded(Value::Null))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

fn be_i16(b: &[u8]) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(..2)?.try_into().ok()?))
}
fn be_i32(b: &[u8]) -> Option<i32> {
    Some(i32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}
fn be_i64(b: &[u8]) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(..8)?.try_into().ok()?))
}

fn decode(ty: &Type, raw: &[u8]) -> Option<Value> {
    if let Kind::Array(elem) = ty.kind() {
        return decode_array(elem, raw);
    }
    if let Kind::Domain(base) = ty.kind() {
        return decode(base, raw);
    }
    if let Kind::Enum(_) = ty.kind() {
        return Some(Value::Text(std::str::from_utf8(raw).ok()?.to_owned()));
    }
    Some(match *ty {
        Type::BOOL => Value::Bool(*raw.first()? != 0),
        Type::INT2 => Value::I64(i64::from(be_i16(raw)?)),
        Type::INT4 => Value::I64(i64::from(be_i32(raw)?)),
        Type::INT8 => Value::I64(be_i64(raw)?),
        Type::OID => Value::I64(i64::from(u32::from_be_bytes(
            raw.get(..4)?.try_into().ok()?,
        ))),
        Type::FLOAT4 => Value::F32(f32::from_be_bytes(raw.get(..4)?.try_into().ok()?)),
        Type::FLOAT8 => Value::F64(f64::from_be_bytes(raw.get(..8)?.try_into().ok()?)),
        Type::NUMERIC => Value::Decimal(decode_numeric(raw)?),
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::UNKNOWN
        | Type::XML
        | Type::CHAR => Value::Text(String::from_utf8(raw.to_vec()).ok()?),
        Type::BYTEA => Value::Bytes(Blob::full(raw.to_vec())),
        Type::UUID => Value::Uuid(raw.get(..16)?.try_into().ok()?),
        Type::JSON => Value::Json(Json {
            text: String::from_utf8(raw.to_vec()).ok()?,
            binary: false,
        }),
        Type::JSONB => Value::Json(Json {
            text: String::from_utf8(raw.get(1..)?.to_vec()).ok()?,
            binary: true,
        }),
        Type::DATE => match be_i32(raw)? {
            i32::MAX => Value::Infinite(Infinite {
                kind: TemporalKind::Date,
                negative: false,
            }),
            i32::MIN => Value::Infinite(Infinite {
                kind: TemporalKind::Date,
                negative: true,
            }),
            d => Value::Date(Date {
                days: d.checked_add(PG_EPOCH_DAYS)?,
            }),
        },
        Type::TIME => Value::Time(Time {
            nanos: u64::try_from(be_i64(raw)?).ok()? * 1_000,
        }),
        // Microseconds since midnight, then the zone in seconds west of UTC.
        Type::TIMETZ => Value::TimeTz(TimeTz {
            time: Time {
                nanos: u64::try_from(be_i64(raw)?).ok()? * 1_000,
            },
            offset_secs: be_i32(raw.get(8..)?)?.checked_neg()?,
        }),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = be_i64(raw)?;
            let tz = *ty == Type::TIMESTAMPTZ;
            let kind = if tz {
                TemporalKind::TimestampTz
            } else {
                TemporalKind::Timestamp
            };
            if micros == i64::MAX || micros == i64::MIN {
                return Some(Value::Infinite(Infinite {
                    kind,
                    negative: micros == i64::MIN,
                }));
            }
            let secs = micros.div_euclid(1_000_000).checked_add(PG_EPOCH_SECS)?;
            let nanos = u32::try_from(micros.rem_euclid(1_000_000) * 1_000).ok()?;
            let ts = Timestamp { secs, nanos };
            if tz {
                Value::TimestampTz(TimestampTz {
                    utc: ts,
                    offset_secs: 0,
                })
            } else {
                Value::Timestamp(ts)
            }
        }
        Type::INTERVAL => Value::Interval(Interval {
            nanos: be_i64(raw)?.checked_mul(1_000)?,
            days: be_i32(raw.get(8..)?)?,
            months: be_i32(raw.get(12..)?)?,
        }),
        _ => return None,
    })
}

/// NUMERIC: ndigits, weight, sign, dscale, then base-10000 digits.
fn decode_numeric(raw: &[u8]) -> Option<String> {
    let ndigits = usize::try_from(be_i16(raw)?).ok()?;
    let weight = i32::from(be_i16(raw.get(2..)?)?);
    let sign = u16::from_be_bytes(raw.get(4..6)?.try_into().ok()?);
    let dscale = usize::try_from(be_i16(raw.get(6..)?)?).ok()?;
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => {}
    }
    let digits: Vec<i32> = (0..ndigits)
        .map(|i| be_i16(raw.get(8 + i * 2..)?).map(i32::from))
        .collect::<Option<_>>()?;
    let mut int = String::new();
    for w in 0..=weight.max(-1) {
        let d = digits.get(usize::try_from(w).ok()?).copied().unwrap_or(0);
        if int.is_empty() {
            if d != 0 {
                int.push_str(&d.to_string());
            }
        } else {
            int.push_str(&format!("{d:04}"));
        }
    }
    if int.is_empty() {
        int.push('0');
    }
    let mut frac = String::new();
    let mut i = weight + 1;
    while frac.len() < dscale {
        let d = if i < 0 {
            0
        } else {
            digits.get(usize::try_from(i).ok()?).copied().unwrap_or(0)
        };
        frac.push_str(&format!("{d:04}"));
        i += 1;
    }
    frac.truncate(dscale);
    let mut s = if sign == 0x4000 {
        format!("-{int}")
    } else {
        int
    };
    if dscale > 0 {
        s.push('.');
        s.push_str(&frac);
    }
    Some(s)
}

/// Encodes a decimal string as NUMERIC.
fn encode_numeric(text: &str, out: &mut BytesMut) -> Option<()> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if body.eq_ignore_ascii_case("nan") {
        out.put_slice(&[0, 0, 0, 0, 0xC0, 0, 0, 0]);
        return Some(());
    }
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit())
        || (int.is_empty() && frac.is_empty())
    {
        return None;
    }
    let dscale = i16::try_from(frac.len()).ok()?;
    let int = int.trim_start_matches('0');
    // Pad the integer part on the left and the fraction on the right to groups of four.
    let int_pad = (4 - int.len() % 4) % 4;
    let int_digits = format!("{}{int}", "0".repeat(int_pad));
    let frac_pad = (4 - frac.len() % 4) % 4;
    let frac_digits = format!("{frac}{}", "0".repeat(frac_pad));
    let mut groups: Vec<i16> = Vec::new();
    for c in int_digits
        .as_bytes()
        .chunks(4)
        .chain(frac_digits.as_bytes().chunks(4))
    {
        groups.push(std::str::from_utf8(c).ok()?.parse().ok()?);
    }
    let int_groups = i16::try_from(int_digits.len() / 4).ok()?;
    let mut weight = int_groups - 1;
    // Strip leading and trailing zero groups.
    while groups.first() == Some(&0) && !groups.is_empty() {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    out.put_i16(i16::try_from(groups.len()).ok()?);
    out.put_i16(weight);
    out.put_u16(if neg && !groups.is_empty() { 0x4000 } else { 0 });
    out.put_i16(dscale);
    for g in groups {
        out.put_i16(g);
    }
    Some(())
}

fn decode_array(elem: &Type, raw: &[u8]) -> Option<Value> {
    let ndim = usize::try_from(be_i32(raw)?).ok()?;
    if ndim == 0 {
        return Some(Value::List(List {
            kind: ListKind::Array,
            items: Vec::new(),
        }));
    }
    let mut dims = Vec::with_capacity(ndim);
    let mut pos = 12;
    for _ in 0..ndim {
        dims.push(usize::try_from(be_i32(raw.get(pos..)?)?).ok()?);
        pos += 8;
    }
    let total: usize = dims.iter().product();
    let mut flat = Vec::with_capacity(total.min(1_000_000));
    for _ in 0..total {
        let len = be_i32(raw.get(pos..)?)?;
        pos += 4;
        if len < 0 {
            flat.push(Value::Null);
        } else {
            let l = usize::try_from(len).ok()?;
            let bytes = raw.get(pos..pos + l)?;
            pos += l;
            flat.push(decode(elem, bytes).unwrap_or_else(|| {
                Value::Other(Other {
                    type_name: elem.name().to_owned(),
                    text: format!("\\x{}", treasury_core::format::hex(bytes)),
                })
            }));
        }
    }
    Some(nest(&dims, &mut flat.into_iter()))
}

fn nest(dims: &[usize], it: &mut impl Iterator<Item = Value>) -> Value {
    let items = match dims {
        [] => Vec::new(),
        [n] => it.take(*n).collect(),
        [n, rest @ ..] => (0..*n).map(|_| nest(rest, it)).collect(),
    };
    Value::List(List {
        kind: ListKind::Array,
        items,
    })
}

/// A bind parameter, encoded for whatever type the server inferred for its placeholder.
#[derive(Debug)]
pub struct Param(pub Value);

fn is_text_type(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN
    )
}

/// `v` in the server's text input syntax, for parameters sent as text.
fn pg_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_owned(),
        Value::Bytes(b) => format!("\\x{}", treasury_core::format::hex(&b.bytes)),
        Value::Json(j) => j.text.clone(),
        Value::List(l) => {
            let items: Vec<String> = l
                .items
                .iter()
                .map(|item| match item {
                    Value::Null => "NULL".to_owned(),
                    Value::List(_) => pg_text(item),
                    other => format!(
                        "\"{}\"",
                        pg_text(other).replace('\\', "\\\\").replace('"', "\\\"")
                    ),
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
        other => treasury_core::format::full_text(other),
    }
}

/// Writes `v` in the binary format of `ty`; false when there is no binary encoding for that
/// pair, in which case the parameter goes as text and the server parses it.
fn encode_binary(ty: &Type, v: &Value, out: &mut BytesMut) -> bool {
    match (ty, v) {
        (t, Value::Text(s)) if is_text_type(t) || *t == Type::XML => out.put_slice(s.as_bytes()),
        (t, other) if is_text_type(t) => out.put_slice(pg_text(other).as_bytes()),
        (&Type::BOOL, Value::Bool(b)) => out.put_u8(u8::from(*b)),
        (&Type::INT2, Value::I64(n)) => match i16::try_from(*n) {
            Ok(n) => out.put_i16(n),
            Err(_) => return false,
        },
        (&Type::INT4, Value::I64(n)) => match i32::try_from(*n) {
            Ok(n) => out.put_i32(n),
            Err(_) => return false,
        },
        (&Type::INT8, Value::I64(n)) => out.put_i64(*n),
        (&Type::FLOAT4, Value::F32(f)) => out.put_f32(*f),
        (&Type::FLOAT8, Value::F64(f)) => out.put_f64(*f),
        (&Type::FLOAT8, Value::F32(f)) => out.put_f64(f64::from(*f)),
        (&Type::NUMERIC, Value::Decimal(s)) => return encode_numeric(s, out).is_some(),
        (&Type::NUMERIC, Value::I64(n)) => return encode_numeric(&n.to_string(), out).is_some(),
        // A partial value would silently write a shortened one.
        (&Type::BYTEA, Value::Bytes(b)) if b.complete => out.put_slice(&b.bytes),
        (&Type::UUID, Value::Uuid(u)) => out.put_slice(u),
        (&Type::JSON, Value::Json(j)) => out.put_slice(j.text.as_bytes()),
        (&Type::JSONB, Value::Json(j)) => {
            out.put_u8(1);
            out.put_slice(j.text.as_bytes());
        }
        (&Type::DATE, Value::Date(d)) => out.put_i32(d.days - PG_EPOCH_DAYS),
        (&Type::TIMESTAMP, Value::Timestamp(ts)) => {
            out.put_i64((ts.secs - PG_EPOCH_SECS) * 1_000_000 + i64::from(ts.nanos / 1_000))
        }
        (&Type::TIMESTAMPTZ, Value::TimestampTz(tz)) => {
            out.put_i64((tz.utc.secs - PG_EPOCH_SECS) * 1_000_000 + i64::from(tz.utc.nanos / 1_000))
        }
        _ => return false,
    }
    true
}

impl Param {
    /// The value for `ty`: text typed in the UI is read the way the server would read it.
    fn typed(&self, ty: &Type) -> Value {
        match &self.0 {
            Value::Text(t) if !is_text_type(ty) => crate::text::parse(ty, t),
            other => other.clone(),
        }
    }

    fn binary(&self, ty: &Type) -> Option<BytesMut> {
        let mut buf = BytesMut::new();
        encode_binary(ty, &self.typed(ty), &mut buf).then_some(buf)
    }
}

impl ToSql for Param {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if self.0.is_null() {
            return Ok(IsNull::Yes);
        }
        if let Value::Bytes(b) = &self.0
            && !b.complete
        {
            return Err(
                "only the start of this binary value was loaded, so it cannot be written back"
                    .into(),
            );
        }
        match self.binary(ty) {
            Some(buf) => out.extend_from_slice(&buf),
            None => out.put_slice(pg_text(&self.0).as_bytes()),
        }
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, ty: &Type) -> Format {
        if self.0.is_null() || self.binary(ty).is_some() {
            Format::Binary
        } else {
            Format::Text
        }
    }

    to_sql_checked!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_numeric(s: &str) -> String {
        let mut b = BytesMut::new();
        encode_numeric(s, &mut b).unwrap();
        decode_numeric(&b).unwrap()
    }

    #[test]
    fn numeric_round_trips() {
        for (input, expected) in [
            ("0", "0"),
            ("1", "1"),
            ("-1", "-1"),
            ("12345678901234567890.0100", "12345678901234567890.0100"),
            ("0.0001", "0.0001"),
            ("10000", "10000"),
            ("-0.5", "-0.5"),
            ("NaN", "NaN"),
            ("00012.30", "12.30"),
        ] {
            assert_eq!(roundtrip_numeric(input), expected, "{input}");
        }
    }

    #[test]
    fn timetz_binary_keeps_the_offset() {
        // 13:45:30+02 as the server sends it: the zone is stored west of UTC.
        let mut raw = (49_530_000_000i64).to_be_bytes().to_vec();
        raw.extend_from_slice(&(-7_200i32).to_be_bytes());
        assert_eq!(
            decode(&Type::TIMETZ, &raw),
            Some(Value::TimeTz(TimeTz {
                time: Time {
                    nanos: 49_530_000_000_000
                },
                offset_secs: 7_200
            }))
        );
        assert_eq!(decode(&Type::TIMETZ, &raw[..10]), None);
    }

    #[test]
    fn temporal_binary() {
        assert_eq!(
            decode(&Type::DATE, &0i32.to_be_bytes()),
            Some(Value::Date(Date {
                days: PG_EPOCH_DAYS
            }))
        );
        assert_eq!(
            decode(&Type::TIMESTAMP, &1_500_000i64.to_be_bytes()),
            Some(Value::Timestamp(Timestamp {
                secs: PG_EPOCH_SECS + 1,
                nanos: 500_000_000
            }))
        );
        assert!(matches!(
            decode(&Type::TIMESTAMPTZ, &i64::MAX.to_be_bytes()),
            Some(Value::Infinite(_))
        ));
    }

    #[test]
    fn arrays_nest() {
        // 2x2 int4 array {{1,2},{3,NULL}}.
        let mut b = BytesMut::new();
        b.put_i32(2);
        b.put_i32(1);
        b.put_u32(23);
        for _ in 0..2 {
            b.put_i32(2);
            b.put_i32(1);
        }
        for v in [Some(1), Some(2), Some(3), None] {
            match v {
                Some(n) => {
                    b.put_i32(4);
                    b.put_i32(n);
                }
                None => b.put_i32(-1),
            }
        }
        let v = decode(&Type::INT4_ARRAY, &b).unwrap();
        let Value::List(outer) = v else { panic!() };
        let Value::List(second) = &outer.items[1] else {
            panic!()
        };
        assert_eq!(second.items, vec![Value::I64(3), Value::Null]);
    }

    #[test]
    fn truncated_input_is_none_not_a_panic() {
        assert!(decode(&Type::INT8, &[1, 2]).is_none());
        assert!(decode_numeric(&[0, 5]).is_none());
        assert!(decode(&Type::INT4_ARRAY, &[0, 0, 0, 1]).is_none());
    }
}
