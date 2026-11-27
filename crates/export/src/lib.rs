//! Exports that stream from a result's cursor straight to a file: CSV, NDJSON, JSON, Parquet and
//! SQL INSERT statements. Rows never pass through the UI and are never all in memory at once.
//!
//! The file is written under a temporary name in the destination folder and moved into place
//! only when it is complete, so a cancelled or failed export (a full disk, say) leaves nothing
//! behind that looks like a finished file.

#![forbid(unsafe_code)]

mod encode;
mod parquet_sink;

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use treasury_core::cursor::{Execution, RowSink};
use treasury_core::driver::SqlDialect;
use treasury_core::exec::{Column, Row};
use treasury_core::{Error, ErrorKind, Result};

/// The export formats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values with a header row.
    Csv {
        /// Field delimiter.
        delimiter: u8,
        /// Text written for NULL.
        null: String,
        /// Whether the first line names the columns.
        header: bool,
    },
    /// One JSON object per line.
    Ndjson,
    /// One JSON array of objects.
    Json,
    /// Apache Parquet.
    Parquet,
    /// A Markdown table, for pasting into notes and issues (copy only).
    Markdown,
    /// INSERT statements for `table` in `dialect`.
    SqlInsert {
        /// The dialect to write.
        dialect: SqlDialect,
        /// Schema of the target table.
        schema: Option<String>,
        /// The target table.
        table: String,
    },
}

impl Format {
    /// Plain CSV: commas, empty fields for NULL.
    pub fn csv() -> Self {
        Format::Csv {
            delimiter: b',',
            null: String::new(),
            header: true,
        }
    }

    /// Values separated by tabs, without a header: what a spreadsheet takes from the clipboard.
    pub fn tsv() -> Self {
        Format::Csv {
            delimiter: b'\t',
            null: String::new(),
            header: false,
        }
    }
}

/// What an export wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Rows written.
    pub rows: u64,
    /// Bytes in the finished file.
    pub bytes: u64,
    /// Where it is.
    pub path: PathBuf,
}

/// A destination for rows that ends by producing the finished file.
pub(crate) trait Sink: RowSink {
    /// Flushes everything; the file is then complete.
    fn finish(self: Box<Self>) -> Result<tempfile::NamedTempFile>;
}

pub(crate) fn io_error(e: &std::io::Error) -> Error {
    let full = e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(28);
    if full {
        Error::new(
            ErrorKind::Internal,
            "The disk is full. The export was stopped and its partial file removed. Free some \
             space or choose another folder.",
        )
    } else {
        Error::new(
            ErrorKind::Internal,
            format!("The export file could not be written: {e}."),
        )
    }
}

fn temp_in(dest: &Path) -> Result<tempfile::NamedTempFile> {
    let dir = dest
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tempfile::Builder::new()
        .prefix(".treasury-export-")
        .tempfile_in(dir)
        .map_err(|e| io_error(&e))
}

/// Streams result set `set` of `execution` into `dest` as `format`. `progress` receives the
/// number of rows written so far. Cancelling the execution stops the export and removes the
/// partial file.
pub async fn export_execution(
    execution: &Execution,
    set: usize,
    dest: &Path,
    format: &Format,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<Report> {
    let tmp = temp_in(dest)?;
    let mut sink = new_sink(format, tmp)?;
    let rows = execution.drain(set, sink.as_mut(), progress).await?;
    let tmp = sink.finish()?;
    finish(tmp, dest, rows)
}

/// Writes rows that are already in memory (a selection in the grid) to `dest`.
pub fn export_rows(
    columns: &[Column],
    rows: &[Row],
    dest: &Path,
    format: &Format,
) -> Result<Report> {
    let tmp = temp_in(dest)?;
    let mut sink = new_sink(format, tmp)?;
    sink.begin(columns)?;
    sink.rows(rows)?;
    let tmp = sink.finish()?;
    finish(tmp, dest, u64::try_from(rows.len()).unwrap_or(u64::MAX))
}

/// Renders rows to text in a text format, for the clipboard.
pub fn render_rows(columns: &[Column], rows: &[Row], format: &Format) -> Result<String> {
    if matches!(format, Format::Parquet) {
        return Err(Error::unsupported(
            "Parquet is a binary format and cannot be copied as text.",
        ));
    }
    let mut out = Vec::new();
    {
        let mut enc = encode::TextEncoder::new(format, &mut out)?;
        enc.begin(columns)?;
        enc.rows(rows)?;
        enc.end()?;
    }
    String::from_utf8(out).map_err(|_| Error::internal("an export produced invalid UTF-8"))
}

fn finish(tmp: tempfile::NamedTempFile, dest: &Path, rows: u64) -> Result<Report> {
    tmp.as_file().sync_all().map_err(|e| io_error(&e))?;
    let file = tmp.persist(dest).map_err(|e| io_error(&e.error))?;
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(Report {
        rows,
        bytes,
        path: dest.to_owned(),
    })
}

fn new_sink(format: &Format, tmp: tempfile::NamedTempFile) -> Result<Box<dyn Sink>> {
    match format {
        Format::Parquet => Ok(Box::new(parquet_sink::ParquetSink::new(tmp))),
        other => Ok(Box::new(TextSink {
            format: other.clone(),
            out: Some(BufWriter::with_capacity(256 * 1024, tmp)),
            columns: Vec::new(),
            started: false,
            first_row: true,
        })),
    }
}

/// CSV, NDJSON, JSON and SQL go through the same text encoder.
struct TextSink {
    format: Format,
    out: Option<BufWriter<tempfile::NamedTempFile>>,
    columns: Vec<Column>,
    started: bool,
    first_row: bool,
}

impl RowSink for TextSink {
    fn begin(&mut self, columns: &[Column]) -> Result<()> {
        self.columns = columns.to_vec();
        let out = self
            .out
            .as_mut()
            .ok_or_else(|| Error::internal("export already finished"))?;
        encode::write_prologue(&self.format, columns, out)?;
        self.started = true;
        Ok(())
    }

    fn rows(&mut self, rows: &[Row]) -> Result<()> {
        let out = self
            .out
            .as_mut()
            .ok_or_else(|| Error::internal("export already finished"))?;
        for row in rows {
            encode::write_row(&self.format, &self.columns, row, self.first_row, out)?;
            self.first_row = false;
        }
        Ok(())
    }
}

impl Sink for TextSink {
    fn finish(mut self: Box<Self>) -> Result<tempfile::NamedTempFile> {
        let mut out = self
            .out
            .take()
            .ok_or_else(|| Error::internal("export already finished"))?;
        if !self.started {
            encode::write_prologue(&self.format, &[], &mut out)?;
        }
        encode::write_epilogue(&self.format, &mut out)?;
        out.flush().map_err(|e| io_error(&e))?;
        out.into_inner().map_err(|e| io_error(e.error()))
    }
}

#[cfg(test)]
mod tests;
