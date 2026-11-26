//! Text encodings: CSV, NDJSON, JSON and SQL INSERT.

use crate::{Format, io_error};
use std::io::Write;
use treasury_core::exec::{Column, Row};
use treasury_core::format::full_text;
use treasury_core::json::to_json;
use treasury_core::value::Value;
use treasury_core::{Error, Result};

/// A CSV field: values as their full text, binary as `\x` hex, nested values as JSON.
fn csv_field(v: &Value, null: &str) -> String {
    match v {
        Value::Null => null.to_owned(),
        Value::List(_) | Value::Composite(_) | Value::Map(_) | Value::Document(_) => {
            to_json(v).to_string()
        }
        other => full_text(other),
    }
}

fn object(columns: &[Column], row: &Row) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let v = row.get(i).map(to_json).unwrap_or(serde_json::Value::Null);
        // Duplicate column names (SELECT a, a) keep both, the second with a suffix.
        let mut key = c.name.clone();
        let mut n = 2;
        while map.contains_key(&key) {
            key = format!("{}_{n}", c.name);
            n += 1;
        }
        map.insert(key, v);
    }
    serde_json::Value::Object(map)
}

fn write(out: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    out.write_all(bytes).map_err(|e| io_error(&e))
}

fn csv_record(delimiter: u8, fields: &[String], out: &mut dyn Write) -> Result<()> {
    let mut w = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .from_writer(Vec::new());
    w.write_record(fields)
        .map_err(|e| Error::internal(format!("CSV encoding failed: {e}")))?;
    let bytes = w
        .into_inner()
        .map_err(|e| Error::internal(format!("CSV encoding failed: {e}")))?;
    write(out, &bytes)
}

/// Writes what comes before the first row (CSV header, JSON opening bracket).
pub fn write_prologue(format: &Format, columns: &[Column], out: &mut dyn Write) -> Result<()> {
    match format {
        Format::Csv { delimiter, .. } => {
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            csv_record(*delimiter, &names, out)
        }
        Format::Json => write(out, b"["),
        Format::Ndjson | Format::SqlInsert { .. } | Format::Parquet => Ok(()),
    }
}

/// Writes one row.
pub fn write_row(
    format: &Format,
    columns: &[Column],
    row: &Row,
    first: bool,
    out: &mut dyn Write,
) -> Result<()> {
    match format {
        Format::Csv { delimiter, null } => {
            let fields: Vec<String> = row.iter().map(|v| csv_field(v, null)).collect();
            csv_record(*delimiter, &fields, out)
        }
        Format::Ndjson => {
            let mut line = serde_json::to_vec(&object(columns, row))
                .map_err(|e| Error::internal(format!("JSON encoding failed: {e}")))?;
            line.push(b'\n');
            write(out, &line)
        }
        Format::Json => {
            if !first {
                write(out, b",")?;
            }
            write(out, b"\n  ")?;
            let obj = serde_json::to_vec(&object(columns, row))
                .map_err(|e| Error::internal(format!("JSON encoding failed: {e}")))?;
            write(out, &obj)
        }
        Format::SqlInsert {
            dialect,
            schema,
            table,
        } => {
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            let mut stmt =
                treasury_sql::render::insert(*dialect, schema.as_deref(), table, &names, row);
            stmt.push('\n');
            write(out, stmt.as_bytes())
        }
        Format::Parquet => Err(Error::internal("Parquet rows go through the Parquet sink")),
    }
}

/// Writes what comes after the last row.
pub fn write_epilogue(format: &Format, out: &mut dyn Write) -> Result<()> {
    match format {
        Format::Json => write(out, b"\n]\n"),
        _ => Ok(()),
    }
}

/// The text encoder used for clipboard copies.
pub struct TextEncoder<'a> {
    format: Format,
    out: &'a mut Vec<u8>,
    columns: Vec<Column>,
    first: bool,
}

impl<'a> TextEncoder<'a> {
    /// An encoder writing `format` into `out`.
    pub fn new(format: &Format, out: &'a mut Vec<u8>) -> Result<Self> {
        Ok(Self {
            format: format.clone(),
            out,
            columns: Vec::new(),
            first: true,
        })
    }

    /// Header.
    pub fn begin(&mut self, columns: &[Column]) -> Result<()> {
        self.columns = columns.to_vec();
        write_prologue(&self.format, columns, self.out)
    }

    /// Rows.
    pub fn rows(&mut self, rows: &[Row]) -> Result<()> {
        for r in rows {
            write_row(&self.format, &self.columns, r, self.first, self.out)?;
            self.first = false;
        }
        Ok(())
    }

    /// Footer.
    pub fn end(&mut self) -> Result<()> {
        write_epilogue(&self.format, self.out)
    }
}
