//! SQL Server values as Treasury's, both ways.

use std::borrow::Cow;
use tiberius::time::{Date as TDate, DateTime2, DateTimeOffset, Time as TTime};
use tiberius::{ColumnData, ColumnType, ToSql, Uuid};
use treasury_core::exec::{Column, ValueKind};
use treasury_core::format::full_text;
use treasury_core::value::{
    Blob, Date, Other, Time, Timestamp, TimestampTz, Value, days_from_civil,
};

/// Binary values keep at most their first megabyte.
const MAX_INLINE_BYTES: usize = 1024 * 1024;

/// Days from 0001-01-01 (SQL Server's `date` epoch) to 1970-01-01.
fn epoch_0001() -> i64 {
    -days_from_civil(1, 1, 1)
}

/// Days from 1900-01-01 (the `datetime` epoch) to 1970-01-01.
fn epoch_1900() -> i64 {
    -days_from_civil(1900, 1, 1)
}

pub(crate) fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Decimaln | ColumnType::Numericn => "decimal",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::Image => "image",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Text => "text",
        ColumnType::NText => "ntext",
        ColumnType::Xml => "xml",
        _ => "sql_variant",
    }
}

pub(crate) fn kind(t: ColumnType) -> ValueKind {
    match t {
        ColumnType::Bit | ColumnType::Bitn => ValueKind::Bool,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => ValueKind::Integer,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => ValueKind::Float,
        ColumnType::Money | ColumnType::Money4 | ColumnType::Decimaln | ColumnType::Numericn => {
            ValueKind::Decimal
        }
        ColumnType::Guid => ValueKind::Uuid,
        ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime4
        | ColumnType::Datetime2 => ValueKind::Timestamp,
        ColumnType::Daten => ValueKind::Date,
        ColumnType::Timen => ValueKind::Time,
        ColumnType::DatetimeOffsetn => ValueKind::TimestampTz,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => ValueKind::Binary,
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText
        | ColumnType::Xml => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

pub(crate) fn column(c: &tiberius::Column) -> Column {
    Column::new(c.name(), type_name(c.column_type()), kind(c.column_type()))
}

/// `value / 10^scale` as an exact decimal string.
pub(crate) fn decimal(value: i128, scale: u8) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let body = if scale == 0 {
        digits
    } else if digits.len() > scale {
        format!(
            "{}.{}",
            &digits[..digits.len() - scale],
            &digits[digits.len() - scale..]
        )
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    };
    if negative { format!("-{body}") } else { body }
}

fn time_nanos(t: TTime) -> u64 {
    let scale = u32::from(t.scale().min(9));
    t.increments() * 10u64.pow(9 - scale)
}

fn timestamp(days_since_0001: u32, nanos_of_day: u64) -> Timestamp {
    let days = i64::from(days_since_0001) - epoch_0001();
    Timestamp {
        secs: days * 86_400 + i64::try_from(nanos_of_day / 1_000_000_000).unwrap_or(0),
        nanos: u32::try_from(nanos_of_day % 1_000_000_000).unwrap_or(0),
    }
}

/// One cell.
pub(crate) fn convert(data: &ColumnData<'static>) -> Value {
    match data {
        ColumnData::U8(v) => v.map_or(Value::Null, |n| Value::I64(i64::from(n))),
        ColumnData::I16(v) => v.map_or(Value::Null, |n| Value::I64(i64::from(n))),
        ColumnData::I32(v) => v.map_or(Value::Null, |n| Value::I64(i64::from(n))),
        ColumnData::I64(v) => v.map_or(Value::Null, Value::I64),
        ColumnData::F32(v) => v.map_or(Value::Null, Value::F32),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::F64),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v
            .as_ref()
            .map_or(Value::Null, |s| Value::Text(s.clone().into_owned())),
        ColumnData::Guid(v) => v.map_or(Value::Null, |u| Value::Uuid(*u.as_bytes())),
        ColumnData::Binary(v) => v.as_ref().map_or(Value::Null, |b| {
            let total = u64::try_from(b.len()).ok();
            let complete = b.len() <= MAX_INLINE_BYTES;
            Value::Bytes(Blob {
                bytes: b[..b.len().min(MAX_INLINE_BYTES)].to_vec(),
                total_len: total,
                complete,
            })
        }),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |n| {
            Value::Decimal(decimal(n.value(), n.scale()))
        }),
        ColumnData::Xml(v) => v
            .as_ref()
            .map_or(Value::Null, |x| Value::Text(x.to_string())),
        ColumnData::DateTime(v) => v.map_or(Value::Null, |dt| {
            // Days since 1900-01-01 and three-hundredths of a second.
            let days = i64::from(dt.days()) - epoch_1900();
            let frac = u64::from(dt.seconds_fragments());
            let nanos_of_day = frac * 1_000_000_000 / 300;
            Value::Timestamp(Timestamp {
                secs: days * 86_400 + i64::try_from(nanos_of_day / 1_000_000_000).unwrap_or(0),
                nanos: u32::try_from(nanos_of_day % 1_000_000_000).unwrap_or(0),
            })
        }),
        ColumnData::SmallDateTime(v) => v.map_or(Value::Null, |dt| {
            let days = i64::from(dt.days()) - epoch_1900();
            Value::Timestamp(Timestamp {
                secs: days * 86_400 + i64::from(dt.seconds_fragments()) * 60,
                nanos: 0,
            })
        }),
        ColumnData::Date(v) => v.map_or(Value::Null, |d| {
            match i32::try_from(i64::from(d.days()) - epoch_0001()) {
                Ok(days) => Value::Date(Date { days }),
                Err(_) => Value::Other(Other {
                    type_name: "date".into(),
                    text: d.days().to_string(),
                }),
            }
        }),
        ColumnData::Time(v) => v.map_or(Value::Null, |t| {
            Value::Time(Time {
                nanos: time_nanos(t),
            })
        }),
        ColumnData::DateTime2(v) => v.map_or(Value::Null, |dt| {
            Value::Timestamp(timestamp(dt.date().days(), time_nanos(dt.time())))
        }),
        ColumnData::DateTimeOffset(v) => v.map_or(Value::Null, |dto| {
            let dt = dto.datetime2();
            Value::TimestampTz(TimestampTz {
                utc: timestamp(dt.date().days(), time_nanos(dt.time())),
                offset_secs: i32::from(dto.offset()) * 60,
            })
        }),
    }
}

/// A bind parameter: a value already in the form the protocol sends.
pub(crate) struct Param(pub(crate) ColumnData<'static>);

impl ToSql for Param {
    fn to_sql(&self) -> ColumnData<'_> {
        self.0.clone()
    }
}

fn date_of(days_since_epoch: i64) -> Option<TDate> {
    u32::try_from(days_since_epoch + epoch_0001())
        .ok()
        .map(TDate::new)
}

fn datetime2(ts: &Timestamp) -> Option<DateTime2> {
    let days = ts.secs.div_euclid(86_400);
    let secs = u64::try_from(ts.secs.rem_euclid(86_400)).ok()?;
    let increments = secs * 10_000_000 + u64::from(ts.nanos / 100);
    Some(DateTime2::new(date_of(days)?, TTime::new(increments, 7)))
}

pub(crate) fn to_param(v: &Value) -> Param {
    let text = |s: String| ColumnData::String(Some(Cow::Owned(s)));
    Param(match v {
        Value::Null => ColumnData::String(None),
        Value::Bool(b) => ColumnData::Bit(Some(*b)),
        Value::I64(n) => ColumnData::I64(Some(*n)),
        Value::U64(n) => match i64::try_from(*n) {
            Ok(n) => ColumnData::I64(Some(n)),
            Err(_) => text(n.to_string()),
        },
        Value::F32(f) => ColumnData::F32(Some(*f)),
        Value::F64(f) => ColumnData::F64(Some(*f)),
        Value::Text(s) | Value::Decimal(s) => text(s.clone()),
        Value::Bytes(b) => ColumnData::Binary(Some(Cow::Owned(b.bytes.clone()))),
        Value::Uuid(u) => ColumnData::Guid(Some(Uuid::from_bytes(*u))),
        Value::Json(j) => text(j.text.clone()),
        Value::Date(d) => match date_of(i64::from(d.days)) {
            Some(date) => ColumnData::Date(Some(date)),
            None => text(full_text(v)),
        },
        Value::Time(t) => ColumnData::Time(Some(TTime::new(t.nanos / 100, 7))),
        Value::Timestamp(ts) => match datetime2(ts) {
            Some(dt) => ColumnData::DateTime2(Some(dt)),
            None => text(full_text(v)),
        },
        Value::TimestampTz(tz) => match (datetime2(&tz.utc), i16::try_from(tz.offset_secs / 60)) {
            (Some(dt), Ok(off)) => ColumnData::DateTimeOffset(Some(DateTimeOffset::new(dt, off))),
            _ => text(full_text(v)),
        },
        other => text(full_text(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_keep_their_scale() {
        assert_eq!(decimal(123_456_789, 4), "12345.6789");
        assert_eq!(decimal(-5, 2), "-0.05");
        assert_eq!(decimal(0, 0), "0");
        assert_eq!(decimal(42, 0), "42");
    }

    #[test]
    fn temporal_values_round_trip_through_parameters() {
        for v in [
            Value::Date(Date { days: 19_782 }),
            Value::Timestamp(Timestamp {
                secs: 1_709_214_330,
                nanos: 123_456_700,
            }),
            Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: 1_709_207_130,
                    nanos: 0,
                },
                offset_secs: 7_200,
            }),
            Value::Time(Time {
                nanos: 49_530_500_000_000,
            }),
        ] {
            assert_eq!(convert(&to_param(&v).0), v);
        }
    }

    #[test]
    fn legacy_datetime_counts_from_1900() {
        let dt = tiberius::time::DateTime::new(0, 300);
        assert_eq!(
            convert(&ColumnData::DateTime(Some(dt))),
            Value::Timestamp(Timestamp {
                secs: -2_208_988_800 + 1,
                nanos: 0
            })
        );
    }
}
