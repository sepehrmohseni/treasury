use super::*;
use futures::StreamExt;
use std::sync::Arc;
use treasury_core::analyze::{Analyzer, StatementClass};
use treasury_core::cache::{DEFAULT_BUDGET, PageStore};
use treasury_core::cursor::ExecutionOptions;
use treasury_core::exec::{ExecEvent, ValueKind};
use treasury_core::value::*;

fn cols() -> Vec<Column> {
    vec![
        Column::new("id", "int8", ValueKind::Integer),
        Column::new("name", "text", ValueKind::Text),
        Column::new("price", "numeric", ValueKind::Decimal),
        Column::new("raw", "bytea", ValueKind::Binary),
        Column::new("day", "date", ValueKind::Date),
        Column::new("seen", "timestamptz", ValueKind::TimestampTz),
        Column::new("tags", "text[]", ValueKind::List),
        Column::new("ok", "bool", ValueKind::Bool),
        Column::new("ratio", "float8", ValueKind::Float),
    ]
}

fn row(i: i64) -> Row {
    vec![
        Value::I64(i),
        Value::Text(format!("item, \"{i}\"\nsecond line")),
        Value::Decimal("12345678901234567890.01".into()),
        Value::Bytes(Blob::full(vec![0xde, 0xad])),
        Value::Date(Date { days: 19_782 }),
        Value::TimestampTz(TimestampTz {
            utc: Timestamp {
                secs: 1_709_211_909,
                nanos: 0,
            },
            offset_secs: 0,
        }),
        Value::List(List {
            kind: ListKind::Array,
            items: vec![Value::Text("a".into()), Value::Null],
        }),
        Value::Bool(i % 2 == 0),
        Value::F64(0.5),
    ]
}

fn null_row() -> Row {
    vec![Value::Null; 9]
}

#[test]
fn csv_quotes_and_marks_null() {
    let text = render_rows(
        &cols(),
        &[row(1), null_row()],
        &Format::Csv {
            delimiter: b',',
            null: "NULL".into(),
            header: true,
        },
    )
    .unwrap();
    let mut r = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers: Vec<String> = r.headers().unwrap().iter().map(str::to_owned).collect();
    assert_eq!(headers[0], "id");
    let recs: Vec<csv::StringRecord> = r.records().map(Result::unwrap).collect();
    assert_eq!(&recs[0][1], "item, \"1\"\nsecond line");
    assert_eq!(&recs[0][2], "12345678901234567890.01");
    assert_eq!(&recs[0][3], "\\xdead");
    assert_eq!(&recs[0][6], "[\"a\",null]");
    assert!(recs[1].iter().all(|f| f == "NULL"));
}

#[test]
fn json_and_ndjson_parse_back() {
    let nd = render_rows(&cols(), &[row(1), row(2)], &Format::Ndjson).unwrap();
    let lines: Vec<serde_json::Value> = nd
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["id"], 2);
    assert_eq!(lines[0]["price"], "12345678901234567890.01");
    assert_eq!(lines[0]["raw"], "3q0=");
    let arr: serde_json::Value =
        serde_json::from_str(&render_rows(&cols(), &[row(1)], &Format::Json).unwrap()).unwrap();
    assert_eq!(arr[0]["day"], "2024-02-29");
    let empty: serde_json::Value =
        serde_json::from_str(&render_rows(&cols(), &[], &Format::Json).unwrap()).unwrap();
    assert_eq!(empty, serde_json::json!([]));
}

#[test]
fn duplicate_column_names_are_kept() {
    let c = vec![
        Column::new("a", "int", ValueKind::Integer),
        Column::new("a", "int", ValueKind::Integer),
    ];
    let nd = render_rows(&c, &[vec![Value::I64(1), Value::I64(2)]], &Format::Ndjson).unwrap();
    let v: serde_json::Value = serde_json::from_str(nd.trim()).unwrap();
    assert_eq!(v, serde_json::json!({ "a": 1, "a_2": 2 }));
}

#[test]
fn sql_inserts_are_single_writes() {
    let format = Format::SqlInsert {
        dialect: SqlDialect::Postgres,
        schema: Some("public".into()),
        table: "items".into(),
    };
    let sql = render_rows(&cols(), &[row(1), null_row()], &format).unwrap();
    let a = treasury_sql::SqlAnalyzer::new(SqlDialect::Postgres);
    let c = a.classify(&sql);
    assert_eq!(c.len(), 2);
    assert!(c.iter().all(|s| s.class == StatementClass::Write));
    assert!(sql.starts_with("INSERT INTO \"public\".\"items\" (\"id\", \"name\""));
}

#[test]
fn parquet_keeps_types() {
    use arrow_array::{
        Array, BooleanArray, Date32Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.parquet");
    let report = export_rows(
        &cols(),
        &[row(1), null_row(), row(3)],
        &path,
        &Format::Parquet,
    )
    .unwrap();
    assert_eq!(report.rows, 3);
    let file = std::fs::File::open(&path).unwrap();
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .build()
        .unwrap();
    let batches: Vec<_> = reader.map(Result::unwrap).collect();
    let b = &batches[0];
    assert_eq!(b.num_rows(), 3);
    let ids = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(ids.value(2), 3);
    assert!(ids.is_null(1));
    let price = b.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(price.value(0), "12345678901234567890.01");
    let day = b.column(4).as_any().downcast_ref::<Date32Array>().unwrap();
    assert_eq!(day.value(0), 19_782);
    let seen = b
        .column(5)
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    assert_eq!(seen.value(0), 1_709_211_909_000_000);
    let ok = b.column(7).as_any().downcast_ref::<BooleanArray>().unwrap();
    assert!(!ok.value(0) && !ok.value(2) && ok.null_count() == 1);
}

#[test]
fn parquet_refuses_values_it_cannot_store() {
    let dir = tempfile::tempdir().unwrap();
    let c = vec![Column::new("n", "bigint unsigned", ValueKind::Integer)];
    let err = export_rows(
        &c,
        &[vec![Value::U64(u64::MAX)]],
        &dir.path().join("x.parquet"),
        &Format::Parquet,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "no partial file left"
    );
}

fn execution(
    events: impl futures::Stream<Item = Result<ExecEvent>> + Send + 'static,
) -> (tempfile::TempDir, Execution) {
    let dir = tempfile::tempdir().unwrap();
    let store = PageStore::new(DEFAULT_BUDGET, dir.path().into()).unwrap();
    let ex = Execution::new(
        events.boxed(),
        store,
        ExecutionOptions {
            page_rows: 1_000,
            max_rows: None,
        },
    );
    (dir, ex)
}

#[tokio::test]
async fn large_results_stream_to_disk() {
    let batches = (0..100).map(|b| {
        Ok(ExecEvent::Rows(
            (0..1_000)
                .map(|i| vec![Value::I64(b * 1_000 + i)])
                .collect(),
        ))
    });
    let events = futures::stream::iter(
        std::iter::once(Ok(ExecEvent::ResultStart(vec![Column::new(
            "n",
            "int8",
            ValueKind::Integer,
        )])))
        .chain(batches)
        .chain(std::iter::once(Ok(ExecEvent::ResultEnd {
            affected: None,
            truncated: false,
        }))),
    );
    let (_d, ex) = execution(events);
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("n.csv");
    let last = std::sync::atomic::AtomicU64::new(0);
    let report = export_execution(&ex, 0, &dest, &Format::csv(), &|n| {
        last.store(n, std::sync::atomic::Ordering::SeqCst);
    })
    .await
    .unwrap();
    assert_eq!(report.rows, 100_000);
    assert_eq!(last.load(std::sync::atomic::Ordering::SeqCst), 100_000);
    let text = std::fs::read_to_string(&dest).unwrap();
    assert_eq!(text.lines().count(), 100_001);
    assert_eq!(text.lines().last(), Some("99999"));
}

#[tokio::test]
async fn cancelling_leaves_no_file() {
    let events = futures::stream::iter(vec![
        Ok(ExecEvent::ResultStart(vec![Column::new(
            "n",
            "int8",
            ValueKind::Integer,
        )])),
        Ok(ExecEvent::Rows(vec![vec![Value::I64(1)]])),
    ])
    .chain(futures::stream::pending());
    let (_d, ex) = execution(events);
    let ex = Arc::new(ex);
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("n.csv");
    let task = {
        let ex = ex.clone();
        let dest = dest.clone();
        tokio::spawn(async move { export_execution(&ex, 0, &dest, &Format::csv(), &|_| {}).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    ex.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(
        std::fs::read_dir(out.path()).unwrap().next().is_none(),
        "no file and no temp file left"
    );
}

#[test]
fn parquet_cannot_be_copied_as_text() {
    assert!(render_rows(&cols(), &[], &Format::Parquet).is_err());
}

#[test]
fn markdown_tables_keep_each_row_on_one_line() {
    let columns = vec![
        Column::new("name", "text", ValueKind::Text),
        Column::new("note", "text", ValueKind::Text),
    ];
    let rows = vec![
        vec![Value::Text("a|b".into()), Value::Text("two\nlines".into())],
        vec![Value::Text("c".into()), Value::Null],
    ];
    let text = render_rows(&columns, &rows, &Format::Markdown).unwrap();
    assert_eq!(
        text,
        "| name | note |\n| --- | --- |\n| a\\|b | two<br>lines |\n| c |  |\n"
    );
}

#[test]
fn tsv_for_the_clipboard_has_no_header() {
    let text = render_rows(&cols(), &[row(1)], &Format::tsv()).unwrap();
    let header = render_rows(&cols(), &[], &Format::csv()).unwrap();
    let first = header.trim_end().split(',').next().unwrap().to_owned();
    assert!(!text.starts_with(&first), "{text}");
    assert!(text.starts_with('1') && text.contains('\t'), "{text}");
}
