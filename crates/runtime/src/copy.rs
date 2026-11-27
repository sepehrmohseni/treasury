//! Copying a range of a result as text, in the forms people paste: values for a spreadsheet,
//! CSV, JSON, a Markdown table, or INSERT statements for the table the rows came from.

use crate::Runtime;
use std::ops::Range;
use treasury_core::driver::QueryLanguage;
use treasury_core::exec::{Column, Row};
use treasury_core::value::Value;
use treasury_core::{Error, Result};
use treasury_export::Format;
use treasury_ipc::{CopyFormat, CopyView};

/// The most rows one copy holds.
pub const COPY_LIMIT: u64 = 10_000;

impl Runtime {
    /// Rows `rows` and columns `columns` (by index) of result set `set`, as text in `format`.
    pub async fn copy(
        &self,
        rid: u64,
        set: usize,
        rows: Range<u64>,
        columns: &[usize],
        format: CopyFormat,
    ) -> Result<CopyView> {
        let r = self.running(rid)?;
        let status = r.execution.status();
        let all = status
            .sets
            .get(set)
            .map(|s| s.columns.clone())
            .ok_or_else(|| Error::internal("that result set does not exist"))?;
        let picked: Vec<Column> = columns
            .iter()
            .map(|&i| {
                all.get(i)
                    .cloned()
                    .ok_or_else(|| Error::internal("that column does not exist"))
            })
            .collect::<Result<_>>()?;
        let wanted = rows.end.saturating_sub(rows.start);
        let count = usize::try_from(wanted.min(COPY_LIMIT)).unwrap_or(usize::MAX);
        let window = r.execution.window(set, rows.start, count).await?;
        let data: Vec<Row> = window
            .rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|&i| row.get(i).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect();
        let format = match format {
            CopyFormat::Tsv => Format::tsv(),
            CopyFormat::Csv => Format::csv(),
            CopyFormat::Json => Format::Json,
            CopyFormat::Markdown => Format::Markdown,
            CopyFormat::Insert => {
                let live = self.live(&r.connection).await?;
                let QueryLanguage::Sql(dialect) = live.session.capabilities().language else {
                    return Err(Error::unsupported(
                        "INSERT statements are for SQL databases.",
                    ));
                };
                let source = live
                    .session
                    .analyzer()
                    .edit_source(&r.text)
                    .map_err(|why| {
                        Error::unsupported(format!(
                            "Copying as INSERT needs rows that come from one table. {why}"
                        ))
                    })?;
                Format::SqlInsert {
                    dialect,
                    schema: source.table.schema,
                    table: source.table.table,
                }
            }
        };
        Ok(CopyView {
            text: treasury_export::render_rows(&picked, &data, &format)?,
            rows: u64::try_from(data.len()).unwrap_or(u64::MAX),
            cut: wanted > COPY_LIMIT,
        })
    }
}
