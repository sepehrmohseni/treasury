//! Parquet export. Columns get a Parquet type from their kind where one maps cleanly (booleans,
//! integers, floats, binary, dates, times, timestamps); everything else is written as text, with
//! nested values as JSON. Decimals are text too, so no digit is ever rounded.

use crate::{Sink, io_error};
use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder,
    StringBuilder, Time64NanosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use std::sync::Arc;
use treasury_core::cursor::RowSink;
use treasury_core::exec::{Column, Row, ValueKind};
use treasury_core::format::full_text;
use treasury_core::json::to_json;
use treasury_core::value::Value;
use treasury_core::{Error, Result};

const BATCH_ROWS: usize = 8_192;

enum Col {
    Bool(BooleanBuilder),
    Int(Int64Builder),
    Float(Float64Builder),
    Bin(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64NanosecondBuilder),
    Ts(TimestampMicrosecondBuilder, bool),
    Text(StringBuilder),
}

impl Col {
    fn for_kind(kind: ValueKind) -> (Self, DataType) {
        match kind {
            ValueKind::Bool => (Col::Bool(BooleanBuilder::new()), DataType::Boolean),
            ValueKind::Integer => (Col::Int(Int64Builder::new()), DataType::Int64),
            ValueKind::Float => (Col::Float(Float64Builder::new()), DataType::Float64),
            ValueKind::Binary => (Col::Bin(BinaryBuilder::new()), DataType::Binary),
            ValueKind::Date => (Col::Date(Date32Builder::new()), DataType::Date32),
            ValueKind::Time => (
                Col::Time(Time64NanosecondBuilder::new()),
                DataType::Time64(TimeUnit::Nanosecond),
            ),
            ValueKind::Timestamp => (
                Col::Ts(TimestampMicrosecondBuilder::new(), false),
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
            ValueKind::TimestampTz => (
                Col::Ts(
                    TimestampMicrosecondBuilder::new().with_timezone("UTC"),
                    true,
                ),
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            ),
            _ => (Col::Text(StringBuilder::new()), DataType::Utf8),
        }
    }

    fn push(&mut self, v: &Value, column: &str) -> Result<()> {
        let mismatch = || {
            Error::new(
                treasury_core::ErrorKind::Unsupported,
                format!(
                    "Column {column} holds a value Parquet cannot store in its type. Export to CSV \
                     or JSON instead."
                ),
            )
        };
        if v.is_null() {
            match self {
                Col::Bool(b) => b.append_null(),
                Col::Int(b) => b.append_null(),
                Col::Float(b) => b.append_null(),
                Col::Bin(b) => b.append_null(),
                Col::Date(b) => b.append_null(),
                Col::Time(b) => b.append_null(),
                Col::Ts(b, _) => b.append_null(),
                Col::Text(b) => b.append_null(),
            }
            return Ok(());
        }
        match (self, v) {
            (Col::Bool(b), Value::Bool(x)) => b.append_value(*x),
            (Col::Int(b), Value::I64(x)) => b.append_value(*x),
            (Col::Int(b), Value::U64(x)) => {
                b.append_value(i64::try_from(*x).map_err(|_| mismatch())?)
            }
            (Col::Int(b), Value::I128(x)) => {
                b.append_value(i64::try_from(*x).map_err(|_| mismatch())?)
            }
            (Col::Int(b), Value::U128(x)) => {
                b.append_value(i64::try_from(*x).map_err(|_| mismatch())?)
            }
            (Col::Float(b), Value::F64(x)) => b.append_value(*x),
            (Col::Float(b), Value::F32(x)) => b.append_value(f64::from(*x)),
            (Col::Bin(b), Value::Bytes(x)) => b.append_value(&x.bytes),
            (Col::Date(b), Value::Date(d)) => b.append_value(d.days),
            (Col::Time(b), Value::Time(t)) => {
                b.append_value(i64::try_from(t.nanos).map_err(|_| mismatch())?)
            }
            (Col::Ts(b, false), Value::Timestamp(ts)) => {
                b.append_value(micros(ts.secs, ts.nanos).ok_or_else(mismatch)?)
            }
            (Col::Ts(b, true), Value::TimestampTz(tz)) => {
                b.append_value(micros(tz.utc.secs, tz.utc.nanos).ok_or_else(mismatch)?)
            }
            (Col::Text(b), other) => {
                let text = match other {
                    Value::List(_) | Value::Composite(_) | Value::Map(_) | Value::Document(_) => {
                        to_json(other).to_string()
                    }
                    _ => full_text(other),
                };
                b.append_value(text);
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Col::Bool(b) => Arc::new(b.finish()),
            Col::Int(b) => Arc::new(b.finish()),
            Col::Float(b) => Arc::new(b.finish()),
            Col::Bin(b) => Arc::new(b.finish()),
            Col::Date(b) => Arc::new(b.finish()),
            Col::Time(b) => Arc::new(b.finish()),
            Col::Ts(b, _) => Arc::new(b.finish()),
            Col::Text(b) => Arc::new(b.finish()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Col::Bool(b) => b.len(),
            Col::Int(b) => b.len(),
            Col::Float(b) => b.len(),
            Col::Bin(b) => b.len(),
            Col::Date(b) => b.len(),
            Col::Time(b) => b.len(),
            Col::Ts(b, _) => b.len(),
            Col::Text(b) => b.len(),
        }
    }
}

fn micros(secs: i64, nanos: u32) -> Option<i64> {
    secs.checked_mul(1_000_000)?
        .checked_add(i64::from(nanos / 1_000))
}

pub(crate) struct ParquetSink {
    tmp: Option<tempfile::NamedTempFile>,
    writer: Option<ArrowWriter<std::fs::File>>,
    schema: Option<Arc<Schema>>,
    names: Vec<String>,
    cols: Vec<Col>,
}

impl ParquetSink {
    pub(crate) fn new(tmp: tempfile::NamedTempFile) -> Self {
        Self {
            tmp: Some(tmp),
            writer: None,
            schema: None,
            names: Vec::new(),
            cols: Vec::new(),
        }
    }

    fn flush_batch(&mut self) -> Result<()> {
        let (Some(schema), Some(writer)) = (&self.schema, self.writer.as_mut()) else {
            return Ok(());
        };
        if self.cols.first().is_none_or(|c| c.len() == 0) {
            return Ok(());
        }
        let arrays: Vec<ArrayRef> = self.cols.iter_mut().map(Col::finish).collect();
        let batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|e| Error::internal(format!("Parquet batch failed: {e}")))?;
        writer.write(&batch).map_err(parquet_error)
    }
}

fn parquet_error(e: parquet::errors::ParquetError) -> Error {
    match e {
        parquet::errors::ParquetError::External(inner) => {
            match inner.downcast::<std::io::Error>() {
                Ok(io) => io_error(&io),
                Err(other) => {
                    Error::internal(format!("The Parquet file could not be written: {other}."))
                }
            }
        }
        other => Error::internal(format!("The Parquet file could not be written: {other}.")),
    }
}

impl RowSink for ParquetSink {
    fn begin(&mut self, columns: &[Column]) -> Result<()> {
        let mut fields = Vec::with_capacity(columns.len());
        let mut seen = std::collections::HashSet::new();
        for c in columns {
            let (col, ty) = Col::for_kind(c.kind);
            let mut name = c.name.clone();
            let mut n = 2;
            while !seen.insert(name.clone()) {
                name = format!("{}_{n}", c.name);
                n += 1;
            }
            fields.push(Field::new(&name, ty, true));
            self.names.push(name);
            self.cols.push(col);
        }
        let schema = Arc::new(Schema::new(fields));
        let file = self
            .tmp
            .as_ref()
            .ok_or_else(|| Error::internal("export already finished"))?
            .reopen()
            .map_err(|e| io_error(&e))?;
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        self.writer =
            Some(ArrowWriter::try_new(file, schema.clone(), Some(props)).map_err(parquet_error)?);
        self.schema = Some(schema);
        Ok(())
    }

    fn rows(&mut self, rows: &[Row]) -> Result<()> {
        for row in rows {
            for (i, col) in self.cols.iter_mut().enumerate() {
                let v = row.get(i).unwrap_or(&Value::Null);
                col.push(v, &self.names[i])?;
            }
            if self.cols.first().is_some_and(|c| c.len() >= BATCH_ROWS) {
                self.flush_batch()?;
            }
        }
        Ok(())
    }
}

impl Sink for ParquetSink {
    fn finish(mut self: Box<Self>) -> Result<tempfile::NamedTempFile> {
        if self.writer.is_none() {
            self.begin(&[])?;
        }
        self.flush_batch()?;
        if let Some(writer) = self.writer.take() {
            writer.close().map_err(parquet_error)?;
        }
        self.tmp
            .take()
            .ok_or_else(|| Error::internal("export already finished"))
    }
}
