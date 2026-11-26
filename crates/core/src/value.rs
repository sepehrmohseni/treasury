//! One value type for every engine: SQL, BSON, CQL and Redis values without loss.
//!
//! A `Value` keeps what the server sent. How it looks on screen is decided separately, in
//! [`crate::format`], so the same value can be displayed, exported, edited and compared without
//! a round trip through text.

use serde::{Deserialize, Serialize};

/// A single cell or field value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    /// SQL NULL, BSON null, a missing Redis key.
    Null,
    /// A boolean.
    Bool(bool),
    /// Any signed integer up to 64 bits.
    I64(i64),
    /// An unsigned 64-bit integer (MySQL BIGINT UNSIGNED, ...).
    U64(u64),
    /// A signed 128-bit integer (Snowflake NUMBER(38,0), Databricks DECIMAL(38,0) that fits).
    I128(i128),
    /// An unsigned 128-bit integer.
    U128(u128),
    /// A single-precision float.
    F32(f32),
    /// A double-precision float.
    F64(f64),
    /// An exact decimal, kept as the server's decimal string (`-12.3400`, `NaN`).
    Decimal(String),
    /// Text.
    Text(String),
    /// Binary data, possibly only its first bytes (see [`Blob`]).
    Bytes(Blob),
    /// A calendar date.
    Date(Date),
    /// A time of day without zone.
    Time(Time),
    /// A time of day with a UTC offset.
    TimeTz(TimeTz),
    /// A date and time without zone (wall-clock).
    Timestamp(Timestamp),
    /// An instant with the offset it was written with.
    TimestampTz(TimestampTz),
    /// A date or timestamp at plus or minus infinity (PostgreSQL).
    Infinite(Infinite),
    /// An interval or duration.
    Interval(Interval),
    /// A UUID, as its 16 bytes.
    Uuid([u8; 16]),
    /// JSON text as the server returned it (key order and number spelling kept).
    Json(Json),
    /// An array, list, set or tuple.
    List(List),
    /// A composite type, row value or user-defined type with named fields.
    Composite(Composite),
    /// A map with arbitrary keys (CQL map, Redis hash).
    Map(Vec<(Value, Value)>),
    /// An ordered document (BSON document).
    Document(Vec<(String, Value)>),
    /// A BSON ObjectId.
    ObjectId([u8; 12]),
    /// A BSON Decimal128: its 16 bytes and the decimal string they encode.
    Decimal128(Decimal128),
    /// BSON types with no SQL counterpart.
    Bson(BsonSpecial),
    /// Anything else, as the engine's text form with its type name (inet, geometry, tsvector...).
    Other(Other),
}

/// Binary data. Large values may hold only their first bytes until the full value is fetched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    /// The bytes held (all of them when `complete`).
    pub bytes: Vec<u8>,
    /// Total length on the server, when known.
    pub total_len: Option<u64>,
    /// Whether `bytes` is the whole value.
    pub complete: bool,
}

impl Blob {
    /// A complete binary value.
    pub fn full(bytes: Vec<u8>) -> Self {
        let total = u64::try_from(bytes.len()).ok();
        Self {
            bytes,
            total_len: total,
            complete: true,
        }
    }
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Date {
    /// Days since the Unix epoch; negative before it.
    pub days: i32,
}

/// Nanoseconds since midnight, below 86 400 s (a leap second is kept as 86 400 s and above).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Time {
    /// Nanoseconds since midnight.
    pub nanos: u64,
}

/// A time of day with the UTC offset it was written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimeTz {
    /// Local time of day.
    pub time: Time,
    /// Offset east of UTC, in seconds.
    pub offset_secs: i32,
}

/// Seconds and nanoseconds since 1970-01-01T00:00:00, read as wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp {
    /// Whole seconds since the epoch; negative before it.
    pub secs: i64,
    /// Nanoseconds within the second, below 1 000 000 000.
    pub nanos: u32,
}

/// An instant (`utc`) and the offset the server reported it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimestampTz {
    /// The instant, in UTC.
    pub utc: Timestamp,
    /// Offset east of UTC, in seconds, for display.
    pub offset_secs: i32,
}

/// Which temporal type an infinite value belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TemporalKind {
    /// A date.
    Date,
    /// A timestamp without zone.
    Timestamp,
    /// A timestamp with zone.
    TimestampTz,
}

/// Plus or minus infinity for a date or timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Infinite {
    /// The type of the column.
    pub kind: TemporalKind,
    /// True for `-infinity`.
    pub negative: bool,
}

/// An interval as months, days and nanoseconds, which is how PostgreSQL and CQL store it: a month
/// and a day are not fixed lengths, so they are not folded into the nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Interval {
    /// Whole months.
    pub months: i32,
    /// Whole days.
    pub days: i32,
    /// Nanoseconds.
    pub nanos: i64,
}

/// JSON exactly as returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Json {
    /// The JSON text.
    pub text: String,
    /// True when the server stores it in a binary form (JSONB), which normalizes key order.
    pub binary: bool,
}

/// What kind of collection a [`List`] is, which matters for display and for export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ListKind {
    /// A SQL array or BSON array.
    Array,
    /// A CQL list.
    List,
    /// A CQL set or Redis set.
    Set,
    /// A CQL tuple.
    Tuple,
}

/// An ordered collection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct List {
    /// The collection kind.
    pub kind: ListKind,
    /// The elements, in server order.
    pub items: Vec<Value>,
}

/// A value with named fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Composite {
    /// The type name, when the server reports one.
    pub type_name: Option<String>,
    /// Fields in declaration order.
    pub fields: Vec<(String, Value)>,
}

/// A BSON Decimal128.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decimal128 {
    /// The 16 bytes as stored, little-endian.
    pub bytes: [u8; 16],
    /// The decimal string they encode.
    pub text: String,
}

/// BSON types that have no general counterpart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BsonSpecial {
    /// A regular expression and its options.
    Regex {
        /// The pattern.
        pattern: String,
        /// Option letters, such as `im`.
        options: String,
    },
    /// JavaScript code.
    JavaScript(String),
    /// The internal replication timestamp.
    Timestamp {
        /// Seconds since the epoch.
        time: u32,
        /// Ordinal within the second.
        increment: u32,
    },
    /// Binary with a subtype other than generic or UUID.
    Binary {
        /// The subtype byte.
        subtype: u8,
        /// The data.
        bytes: Vec<u8>,
    },
    /// The deprecated `undefined`.
    Undefined,
    /// Sorts before every other value.
    MinKey,
    /// Sorts after every other value.
    MaxKey,
    /// The deprecated symbol type.
    Symbol(String),
}

/// A value Treasury has no structured form for, in the engine's own text representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Other {
    /// The engine's type name.
    pub type_name: String,
    /// The text representation.
    pub text: String,
}

impl Value {
    /// True for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// An estimate of the heap and inline bytes this value occupies, used for the page cache's
    /// byte budget. It errs on the high side.
    pub fn approx_size(&self) -> usize {
        const BASE: usize = std::mem::size_of::<Value>();
        BASE + match self {
            Value::Decimal(s) | Value::Text(s) => s.capacity(),
            Value::Bytes(b) => b.bytes.capacity(),
            Value::Json(j) => j.text.capacity(),
            Value::List(l) => l.items.iter().map(Value::approx_size).sum(),
            Value::Composite(c) => c
                .fields
                .iter()
                .map(|(k, v)| k.capacity() + v.approx_size())
                .sum::<usize>(),
            Value::Map(m) => m
                .iter()
                .map(|(k, v)| k.approx_size() + v.approx_size())
                .sum(),
            Value::Document(d) => d.iter().map(|(k, v)| k.capacity() + v.approx_size()).sum(),
            Value::Decimal128(d) => d.text.capacity(),
            Value::Bson(BsonSpecial::Regex { pattern, options }) => {
                pattern.capacity() + options.capacity()
            }
            Value::Bson(BsonSpecial::JavaScript(s) | BsonSpecial::Symbol(s)) => s.capacity(),
            Value::Bson(BsonSpecial::Binary { bytes, .. }) => bytes.capacity(),
            Value::Other(o) => o.type_name.capacity() + o.text.capacity(),
            _ => 0,
        }
    }
}

/// Converts a civil date to days since 1970-01-01 (proleptic Gregorian).
///
/// Howard Hinnant's `days_from_civil`; exact for every `i32` year range Treasury can see.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts days since 1970-01-01 to `(year, month, day)`; the inverse of [`days_from_civil`].
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    // `d` is in 1..=31 and `m` in 1..=12 by construction.
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
        assert_eq!(
            civil_from_days(days_from_civil(-4713, 11, 24)),
            (-4713, 11, 24)
        );
    }

    proptest! {
        #[test]
        fn civil_round_trip(days in -1_000_000_000i64..1_000_000_000) {
            let (y, m, d) = civil_from_days(days);
            prop_assert!((1..=12).contains(&m));
            prop_assert!((1..=31).contains(&d));
            prop_assert_eq!(days_from_civil(y, m, d), days);
        }

        #[test]
        fn value_round_trips_through_messagepack(v in arb_value()) {
            let bytes = rmp_serde::to_vec_named(&v).unwrap();
            let back: Value = rmp_serde::from_slice(&bytes).unwrap();
            prop_assert_eq!(normalize_nan(&back), normalize_nan(&v));
        }

        #[test]
        fn value_round_trips_through_json(v in arb_value()) {
            let text = serde_json::to_string(&v).unwrap();
            let back: Value = serde_json::from_str(&text).unwrap();
            prop_assert_eq!(normalize_nan(&back), normalize_nan(&v));
        }
    }

    /// NaN is never equal to itself; compare it by bit pattern class instead.
    fn normalize_nan(v: &Value) -> Value {
        match v {
            Value::F32(f) if f.is_nan() => Value::Text("NaN32".into()),
            Value::F64(f) if f.is_nan() => Value::Text("NaN64".into()),
            other => other.clone(),
        }
    }

    pub(crate) fn arb_scalar() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::I64),
            any::<u64>().prop_map(Value::U64),
            any::<i128>().prop_map(Value::I128),
            any::<u128>().prop_map(Value::U128),
            any::<f64>()
                .prop_filter("finite for JSON", |f| f.is_finite())
                .prop_map(Value::F64),
            "-?[0-9]{1,30}(\\.[0-9]{1,10})?".prop_map(Value::Decimal),
            ".{0,40}".prop_map(Value::Text),
            proptest::collection::vec(any::<u8>(), 0..64).prop_map(|b| Value::Bytes(Blob::full(b))),
            any::<i32>().prop_map(|days| Value::Date(Date { days })),
            (0u64..86_400_000_000_000).prop_map(|nanos| Value::Time(Time { nanos })),
            (any::<i64>(), 0u32..1_000_000_000)
                .prop_map(|(secs, nanos)| Value::Timestamp(Timestamp { secs, nanos })),
            (any::<i64>(), 0u32..1_000_000_000, -64_800i32..=64_800).prop_map(
                |(secs, nanos, offset_secs)| Value::TimestampTz(TimestampTz {
                    utc: Timestamp { secs, nanos },
                    offset_secs
                })
            ),
            (any::<i32>(), any::<i32>(), any::<i64>()).prop_map(|(months, days, nanos)| {
                Value::Interval(Interval {
                    months,
                    days,
                    nanos,
                })
            }),
            any::<[u8; 16]>().prop_map(Value::Uuid),
            any::<[u8; 12]>().prop_map(Value::ObjectId),
            ("[a-z]{0,8}", any::<bool>()).prop_map(|(k, binary)| Value::Json(Json {
                text: format!("{{\"{k}\":1}}"),
                binary
            })),
        ]
    }

    pub(crate) fn arb_value() -> impl Strategy<Value = Value> {
        arb_scalar().prop_recursive(3, 24, 6, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(|items| Value::List(
                    List {
                        kind: ListKind::Array,
                        items
                    }
                )),
                proptest::collection::vec(("[a-z]{1,6}", inner.clone()), 0..6)
                    .prop_map(Value::Document),
                proptest::collection::vec((inner.clone(), inner), 0..4).prop_map(Value::Map),
            ]
        })
    }
}
