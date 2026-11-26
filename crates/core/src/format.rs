//! Display formatting, kept apart from [`Value`] so a value is never changed by how it is shown.

use crate::value::{
    BsonSpecial, Date, Infinite, Interval, ListKind, Time, Timestamp, Value, civil_from_days,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// How cells are rendered for the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatOptions {
    /// Characters of text shown before the cell is marked truncated.
    pub max_chars: usize,
    /// Bytes of binary shown as hex before the cell is marked truncated.
    pub max_bytes: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            max_chars: 1_000,
            max_bytes: 32,
        }
    }
}

impl FormatOptions {
    /// No truncation: the full text, for the cell viewer and for text exports.
    pub const FULL: Self = Self {
        max_chars: usize::MAX,
        max_bytes: usize::MAX,
    };
}

/// A rendered cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The text to show. For NULL this is empty and `null` is set; the UI draws NULL itself so it
    /// can never be confused with the string "NULL" or with an empty string.
    pub text: String,
    /// The value is NULL.
    pub null: bool,
    /// The text was cut short; the cell viewer shows the rest.
    pub truncated: bool,
}

/// Renders a value for display.
pub fn display(value: &Value, opts: &FormatOptions) -> Cell {
    if value.is_null() {
        return Cell {
            text: String::new(),
            null: true,
            truncated: false,
        };
    }
    let mut out = Writer::new(opts.max_chars);
    write_value(&mut out, value, opts, false);
    Cell {
        truncated: out.truncated,
        text: out.buf,
        null: false,
    }
}

/// The full text of a value, never truncated. NULL renders as `NULL`.
pub fn full_text(value: &Value) -> String {
    let mut out = Writer::new(usize::MAX);
    write_value(&mut out, value, &FormatOptions::FULL, false);
    out.buf
}

/// Formats a date as ISO 8601 (`2024-02-29`, `-0044-03-15`, `+10000-01-01`).
pub fn format_date(d: Date) -> String {
    let mut s = String::with_capacity(10);
    push_date(&mut s, i64::from(d.days));
    s
}

/// Formats a time of day (`13:05:09`, `13:05:09.25`).
pub fn format_time(t: Time) -> String {
    let mut s = String::with_capacity(15);
    push_time(&mut s, t.nanos);
    s
}

/// Formats a wall-clock timestamp (`2024-02-29 13:05:09.123`).
pub fn format_timestamp(ts: Timestamp) -> String {
    let mut s = String::with_capacity(26);
    push_timestamp(&mut s, ts);
    s
}

/// Formats an offset as `+05:30`.
pub fn format_offset(offset_secs: i32) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let mut s = format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60);
    if !abs.is_multiple_of(60) {
        let _ = write!(s, ":{:02}", abs % 60);
    }
    s
}

/// Formats 16 bytes as a lowercase hyphenated UUID.
pub fn format_uuid(b: &[u8; 16]) -> String {
    let h = hex(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// Formats an interval as `1 year 2 months 3 days 04:05:06.5`, omitting zero parts.
pub fn format_interval(iv: Interval) -> String {
    let mut parts: Vec<String> = Vec::new();
    let years = iv.months / 12;
    let months = iv.months % 12;
    let plural = |n: i32, one: &str, many: &str| {
        if n.abs() == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {many}")
        }
    };
    if years != 0 {
        parts.push(plural(years, "year", "years"));
    }
    if months != 0 {
        parts.push(plural(months, "month", "months"));
    }
    if iv.days != 0 {
        parts.push(plural(iv.days, "day", "days"));
    }
    if iv.nanos != 0 || parts.is_empty() {
        let mut t = String::new();
        if iv.nanos < 0 {
            t.push('-');
        }
        let abs = iv.nanos.unsigned_abs();
        let secs = abs / 1_000_000_000;
        let _ = write!(
            t,
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        );
        push_fraction(&mut t, abs % 1_000_000_000);
        parts.push(t);
    }
    parts.join(" ")
}

/// A `String` that stops accepting text past a character budget and remembers that it did.
struct Writer {
    buf: String,
    left: usize,
    truncated: bool,
}

impl Writer {
    fn new(max_chars: usize) -> Self {
        Self {
            buf: String::new(),
            left: max_chars,
            truncated: false,
        }
    }

    fn push(&mut self, s: &str) {
        if self.truncated {
            return;
        }
        let n = s.chars().count();
        if n <= self.left {
            self.buf.push_str(s);
            self.left -= n;
        } else {
            self.buf.extend(s.chars().take(self.left));
            self.left = 0;
            self.truncated = true;
        }
    }

    fn full(&self) -> bool {
        self.truncated
    }
}

fn write_value(out: &mut Writer, value: &Value, opts: &FormatOptions, nested: bool) {
    if out.full() {
        return;
    }
    match value {
        Value::Null => out.push(if nested { "null" } else { "NULL" }),
        Value::Bool(b) => out.push(if *b { "true" } else { "false" }),
        Value::I64(n) => out.push(&n.to_string()),
        Value::U64(n) => out.push(&n.to_string()),
        Value::I128(n) => out.push(&n.to_string()),
        Value::U128(n) => out.push(&n.to_string()),
        Value::F32(f) => out.push(&float(f64::from(*f), f.to_string())),
        Value::F64(f) => out.push(&float(*f, f.to_string())),
        Value::Decimal(s) => out.push(s),
        Value::Text(s) => {
            if nested {
                out.push(&quote(s));
            } else {
                out.push(s);
            }
        }
        Value::Bytes(b) => {
            let shown = b.bytes.len().min(opts.max_bytes);
            let mut s = String::from("\\x");
            s.push_str(&hex(&b.bytes[..shown]));
            out.push(&s);
            let total = b
                .total_len
                .unwrap_or_else(|| u64::try_from(b.bytes.len()).unwrap_or(u64::MAX));
            if shown < b.bytes.len() || !b.complete {
                out.push(&format!(" ({})", human_bytes(total)));
                out.truncated = true;
            }
        }
        Value::Date(d) => out.push(&format_date(*d)),
        Value::Time(t) => out.push(&format_time(*t)),
        Value::TimeTz(t) => {
            out.push(&format_time(t.time));
            out.push(&format_offset(t.offset_secs));
        }
        Value::Timestamp(ts) => out.push(&format_timestamp(*ts)),
        Value::TimestampTz(tz) => {
            let local = Timestamp {
                secs: tz.utc.secs.saturating_add(i64::from(tz.offset_secs)),
                nanos: tz.utc.nanos,
            };
            out.push(&format_timestamp(local));
            out.push(&format_offset(tz.offset_secs));
        }
        Value::Infinite(Infinite { negative, .. }) => {
            out.push(if *negative { "-infinity" } else { "infinity" })
        }
        Value::Interval(iv) => out.push(&format_interval(*iv)),
        Value::Uuid(u) => out.push(&format_uuid(u)),
        Value::Json(j) => out.push(&j.text),
        Value::List(l) => {
            let (open, close) = match l.kind {
                ListKind::Array | ListKind::List => ("[", "]"),
                ListKind::Set => ("{", "}"),
                ListKind::Tuple => ("(", ")"),
            };
            out.push(open);
            for (i, item) in l.items.iter().enumerate() {
                if i > 0 {
                    out.push(", ");
                }
                write_value(out, item, opts, true);
            }
            out.push(close);
        }
        Value::Composite(c) => {
            out.push("(");
            for (i, (name, v)) in c.fields.iter().enumerate() {
                if i > 0 {
                    out.push(", ");
                }
                out.push(name);
                out.push(": ");
                write_value(out, v, opts, true);
            }
            out.push(")");
        }
        Value::Map(m) => {
            out.push("{");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(", ");
                }
                write_value(out, k, opts, true);
                out.push(": ");
                write_value(out, v, opts, true);
            }
            out.push("}");
        }
        Value::Document(d) => {
            out.push("{");
            for (i, (k, v)) in d.iter().enumerate() {
                out.push(if i > 0 { ", " } else { " " });
                out.push(&quote(k));
                out.push(": ");
                write_value(out, v, opts, true);
            }
            out.push(if d.is_empty() { "}" } else { " }" });
        }
        Value::ObjectId(id) => out.push(&format!("ObjectId('{}')", hex(id))),
        Value::Decimal128(d) => out.push(&d.text),
        Value::Bson(b) => out.push(&bson_special(b)),
        Value::Other(o) => out.push(&o.text),
    }
}

fn float(f: f64, shortest: String) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    } else {
        shortest
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn bson_special(b: &BsonSpecial) -> String {
    match b {
        BsonSpecial::Regex { pattern, options } => format!("/{pattern}/{options}"),
        BsonSpecial::JavaScript(code) => code.clone(),
        BsonSpecial::Timestamp { time, increment } => format!("Timestamp({time}, {increment})"),
        BsonSpecial::Binary { subtype, bytes } => {
            format!("BinData({subtype}, '{}')", hex(bytes))
        }
        BsonSpecial::Undefined => "undefined".into(),
        BsonSpecial::MinKey => "MinKey".into(),
        BsonSpecial::MaxKey => "MaxKey".into(),
        BsonSpecial::Symbol(s) => s.clone(),
    }
}

fn push_date(s: &mut String, days: i64) {
    let (y, m, d) = civil_from_days(days);
    if (0..=9999).contains(&y) {
        let _ = write!(s, "{y:04}-{m:02}-{d:02}");
    } else if y < 0 {
        let _ = write!(s, "-{:04}-{m:02}-{d:02}", y.unsigned_abs());
    } else {
        let _ = write!(s, "+{y}-{m:02}-{d:02}");
    }
}

fn push_time(s: &mut String, nanos: u64) {
    let secs = nanos / 1_000_000_000;
    let _ = write!(
        s,
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    );
    push_fraction(s, nanos % 1_000_000_000);
}

fn push_timestamp(s: &mut String, ts: Timestamp) {
    let days = ts.secs.div_euclid(86_400);
    let secs_of_day = ts.secs.rem_euclid(86_400).unsigned_abs();
    push_date(s, days);
    s.push(' ');
    push_time(s, secs_of_day * 1_000_000_000 + u64::from(ts.nanos));
}

/// Appends `.fraction` without trailing zeros, or nothing for zero.
fn push_fraction(s: &mut String, nanos: u64) {
    if nanos == 0 {
        return;
    }
    let digits = format!("{nanos:09}");
    s.push('.');
    s.push_str(digits.trim_end_matches('0'));
}

/// `1.5 KB`, `12 MB`: decimal units, one decimal place below 10.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::*;

    fn show(v: Value) -> String {
        display(&v, &FormatOptions::default()).text
    }

    #[test]
    fn null_is_flagged_not_spelled() {
        let c = display(&Value::Null, &FormatOptions::default());
        assert!(c.null);
        assert_eq!(c.text, "");
        let empty = display(&Value::Text(String::new()), &FormatOptions::default());
        assert!(!empty.null);
        assert_eq!(empty.text, "");
        let word = display(&Value::Text("NULL".into()), &FormatOptions::default());
        assert!(!word.null);
    }

    #[test]
    fn temporal_values() {
        assert_eq!(show(Value::Date(Date { days: 19_782 })), "2024-02-29");
        assert_eq!(
            show(Value::Date(Date {
                days: i32::try_from(days_from_civil(-44, 3, 15)).unwrap()
            })),
            "-0044-03-15"
        );
        assert_eq!(
            show(Value::Time(Time {
                nanos: 13 * 3_600_000_000_000 + 5 * 60_000_000_000 + 9_250_000_000
            })),
            "13:05:09.25"
        );
        assert_eq!(
            show(Value::Timestamp(Timestamp {
                secs: 1_709_211_909,
                nanos: 123_000_000
            })),
            "2024-02-29 13:05:09.123"
        );
        assert_eq!(
            show(Value::TimestampTz(TimestampTz {
                utc: Timestamp {
                    secs: 1_709_211_909,
                    nanos: 0
                },
                offset_secs: 19_800
            })),
            "2024-02-29 18:35:09+05:30"
        );
        assert_eq!(
            show(Value::Timestamp(Timestamp {
                secs: -1,
                nanos: 500_000_000
            })),
            "1969-12-31 23:59:59.5"
        );
        assert_eq!(
            show(Value::Infinite(Infinite {
                kind: TemporalKind::Date,
                negative: true
            })),
            "-infinity"
        );
    }

    #[test]
    fn intervals() {
        assert_eq!(
            format_interval(Interval {
                months: 14,
                days: 3,
                nanos: 4 * 3_600_000_000_000 + 500_000_000
            }),
            "1 year 2 months 3 days 04:00:00.5"
        );
        assert_eq!(
            format_interval(Interval {
                months: 0,
                days: 0,
                nanos: 0
            }),
            "00:00:00"
        );
        assert_eq!(
            format_interval(Interval {
                months: 0,
                days: -1,
                nanos: 0
            }),
            "-1 day"
        );
    }

    #[test]
    fn text_truncates_on_character_boundaries() {
        let opts = FormatOptions {
            max_chars: 3,
            max_bytes: 4,
        };
        let c = display(&Value::Text("héllo".into()), &opts);
        assert_eq!(c.text, "hél");
        assert!(c.truncated);
    }

    #[test]
    fn binary_shows_hex_and_size_when_cut() {
        let opts = FormatOptions {
            max_chars: 100,
            max_bytes: 2,
        };
        let c = display(
            &Value::Bytes(Blob::full(vec![0xde, 0xad, 0xbe, 0xef])),
            &opts,
        );
        assert_eq!(c.text, "\\xdead (4 bytes)");
        assert!(c.truncated);
        let lazy = Blob {
            bytes: vec![1],
            total_len: Some(2_400_000),
            complete: false,
        };
        let c = display(&Value::Bytes(lazy), &FormatOptions::default());
        assert_eq!(c.text, "\\x01 (2.4 MB)");
        assert!(c.truncated);
    }

    #[test]
    fn nested_values_quote_strings() {
        let doc = Value::Document(vec![
            ("_id".into(), Value::ObjectId([0xab; 12])),
            (
                "tags".into(),
                Value::List(List {
                    kind: ListKind::Array,
                    items: vec![Value::Text("a\"b".into()), Value::Null],
                }),
            ),
        ]);
        assert_eq!(
            show(doc),
            "{ \"_id\": ObjectId('abababababababababababab'), \"tags\": [\"a\\\"b\", null] }"
        );
    }

    #[test]
    fn floats_and_uuids() {
        assert_eq!(show(Value::F64(f64::NAN)), "NaN");
        assert_eq!(show(Value::F64(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(show(Value::F64(0.1)), "0.1");
        let mut u = [0u8; 16];
        u[15] = 1;
        assert_eq!(show(Value::Uuid(u)), "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(999), "999 bytes");
        assert_eq!(human_bytes(1_500), "1.5 KB");
        assert_eq!(human_bytes(64_000_000), "64 MB");
    }
}
