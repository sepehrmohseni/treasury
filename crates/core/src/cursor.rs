//! A running or finished execution: its result sets, fetched page by page into the shared
//! [`PageStore`] as the UI scrolls, never further.

use crate::cache::{PageKey, PageStore};
use crate::error::{Error, Result};
use crate::exec::{Column, ExecEvent, ExecStream, Notice, Row};
pub use crate::exec::{ExecutionStatus, SetStatus};
use crate::task::guarded;
use futures::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;
use tokio::sync::watch;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// How many notices one execution keeps; beyond this they are counted, not stored.
const MAX_NOTICES: usize = 1_000;

/// Settings for one execution.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionOptions {
    /// Rows per page.
    pub page_rows: usize,
    /// Stop after this many rows in a result set (the "stop fetching" form of the row limit).
    pub max_rows: Option<u64>,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            page_rows: 500,
            max_rows: None,
        }
    }
}

/// Rows of one result set, starting at `start`.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// Index of the first row.
    pub start: u64,
    /// The rows.
    pub rows: Vec<Row>,
}

/// Receives rows for an export.
pub trait RowSink: Send {
    /// Called once, before any rows.
    fn begin(&mut self, columns: &[Column]) -> Result<()>;
    /// Called with consecutive batches of rows.
    fn rows(&mut self, rows: &[Row]) -> Result<()>;
}

struct SetState {
    columns: Vec<Column>,
    full_pages: u32,
    tail: Vec<Row>,
    rows: u64,
    ended: bool,
    limited: bool,
    affected: Option<u64>,
    exported_rest: bool,
}

impl SetState {
    fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            full_pages: 0,
            tail: Vec::new(),
            rows: 0,
            ended: false,
            limited: false,
            affected: None,
            exported_rest: false,
        }
    }
}

struct Shared {
    sets: Vec<SetState>,
    notices: Vec<Notice>,
    dropped_notices: u64,
    finished: bool,
    error: Option<Error>,
    /// When the last event came from the server, or the reading ended.
    last_event: Option<Instant>,
}

/// A statement's results.
pub struct Execution {
    id: u64,
    store: Arc<PageStore>,
    options: ExecutionOptions,
    stream: tokio::sync::Mutex<Option<ExecStream>>,
    shared: Mutex<Shared>,
    cancel_tx: watch::Sender<bool>,
}

impl Execution {
    /// Wraps a driver's event stream. Nothing is read until rows are asked for.
    pub fn new(stream: ExecStream, store: Arc<PageStore>, options: ExecutionOptions) -> Self {
        let (cancel_tx, _) = watch::channel(false);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            store,
            options: ExecutionOptions {
                page_rows: options.page_rows.max(1),
                ..options
            },
            stream: tokio::sync::Mutex::new(Some(stream)),
            shared: Mutex::new(Shared {
                sets: Vec::new(),
                notices: Vec::new(),
                dropped_notices: 0,
                finished: false,
                error: None,
                last_event: None,
            }),
            cancel_tx,
        }
    }

    /// This execution's identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current state, without waiting for the server.
    pub fn status(&self) -> ExecutionStatus {
        let s = self.lock();
        ExecutionStatus {
            id: self.id,
            sets: s
                .sets
                .iter()
                .map(|set| SetStatus {
                    columns: set.columns.clone(),
                    rows: set.rows,
                    complete: set.ended && !set.limited,
                    limited: set.limited,
                    affected: set.affected,
                    exported_rest: set.exported_rest,
                })
                .collect(),
            notices: s.notices.clone(),
            dropped_notices: s.dropped_notices,
            finished: s.finished,
            error: s.error.clone(),
        }
    }

    /// When the server last sent something for this statement, or reading it ended. Results are
    /// read only as far as they are shown, so this, not the time the result is closed, says how
    /// long the statement took to give what was read.
    pub fn last_event(&self) -> Option<Instant> {
        self.lock().last_event
    }

    /// Stops reading. A fetch in progress ends with a cancelled error; the stream is dropped,
    /// which closes the server-side cursor. The driver's own cancel is the caller's job.
    pub fn cancel(&self) {
        self.cancel_tx.send_replace(true);
    }

    /// Reads until the first result set has a full page or the statement finished, and keeps
    /// going through result sets that end quickly, so short multi-result batches show every
    /// result at once.
    pub async fn prime(&self) -> Result<()> {
        loop {
            let (finished, need_more) = {
                let s = self.lock();
                let busy_set = s.sets.last().is_some_and(|set| !set.ended);
                let page_full = s.sets.last().is_some_and(|set| {
                    set.rows >= u64::try_from(self.options.page_rows).unwrap_or(u64::MAX)
                });
                (s.finished, !(busy_set && page_full))
            };
            if finished || !need_more {
                return self.result_of_stream();
            }
            if !self.pull_one().await? {
                return self.result_of_stream();
            }
        }
    }

    fn result_of_stream(&self) -> Result<()> {
        match &self.lock().error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Up to `count` rows of result set `set` starting at row `start`, fetching from the server
    /// as far as needed.
    pub async fn window(&self, set: usize, start: u64, count: usize) -> Result<Window> {
        let want = start.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        loop {
            let ready = {
                let s = self.lock();
                match s.sets.get(set) {
                    Some(st) => st.rows >= want || st.ended || st.exported_rest || s.finished,
                    None => s.finished,
                }
            };
            if ready {
                break;
            }
            if !self.pull_one().await? {
                break;
            }
        }
        self.cached_rows(set, start, count)
    }

    fn cached_rows(&self, set: usize, start: u64, count: usize) -> Result<Window> {
        let page_rows = u64::try_from(self.options.page_rows).unwrap_or(u64::MAX);
        let (full_pages, tail, total) = {
            let s = self.lock();
            let Some(st) = s.sets.get(set) else {
                return Ok(Window {
                    start,
                    rows: Vec::new(),
                });
            };
            (st.full_pages, st.tail.clone(), st.rows)
        };
        let end = total.min(start.saturating_add(u64::try_from(count).unwrap_or(u64::MAX)));
        let mut rows = Vec::with_capacity(usize::try_from(end.saturating_sub(start)).unwrap_or(0));
        let mut i = start;
        while i < end {
            let page = i / page_rows;
            let offset = usize::try_from(i % page_rows).unwrap_or(0);
            let page_u32 = u32::try_from(page).map_err(|_| Error::internal("too many pages"))?;
            let take = usize::try_from((end - i).min(page_rows - (i % page_rows))).unwrap_or(0);
            if page_u32 < full_pages {
                let data = self.store.get(PageKey {
                    result: self.id,
                    set: u32::try_from(set).unwrap_or(u32::MAX),
                    page: page_u32,
                })?;
                rows.extend(data.iter().skip(offset).take(take).cloned());
            } else {
                rows.extend(tail.iter().skip(offset).take(take).cloned());
            }
            i += u64::try_from(take.max(1)).unwrap_or(1);
        }
        Ok(Window { start, rows })
    }

    /// Writes every row of result set `set` to `sink`: the rows already fetched, then the rest
    /// straight from the server without keeping them. `progress` is called with the running
    /// total. Returns the number of rows written.
    pub async fn drain(
        &self,
        set: usize,
        sink: &mut dyn RowSink,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64> {
        // Make sure the set exists.
        while self.lock().sets.len() <= set {
            if !self.pull_one().await? {
                break;
            }
        }
        let (columns, kept) = {
            let s = self.lock();
            let st = s
                .sets
                .get(set)
                .ok_or_else(|| Error::internal("no such result set"))?;
            (st.columns.clone(), st.rows)
        };
        sink.begin(&columns)?;
        let mut written = 0u64;
        let chunk = self.options.page_rows;
        let mut start = 0u64;
        while start < kept {
            let w = self.cached_rows(set, start, chunk)?;
            if w.rows.is_empty() {
                break;
            }
            sink.rows(&w.rows)?;
            let n = u64::try_from(w.rows.len()).unwrap_or(0);
            written += n;
            start += n;
            progress(written);
        }
        // The rest comes straight from the stream, if this set is the one being read.
        let streaming = {
            let s = self.lock();
            s.sets.len() == set + 1 && !s.sets[set].ended && !s.finished
        };
        if !streaming {
            return Ok(written);
        }
        let mut guard = self.stream.lock().await;
        let mut cancel = self.cancel_tx.subscribe();
        while let Some(stream) = guard.as_mut() {
            let event = tokio::select! {
                e = guarded("Reading rows", async { Ok(stream.next().await) }) => e?,
                _ = cancel.wait_for(|c| *c) => {
                    *guard = None;
                    let err = Error::cancelled();
                    self.finish(Some(err.clone()));
                    return Err(err);
                }
            };
            match event {
                Some(Ok(ExecEvent::Rows(rows))) => {
                    sink.rows(&rows)?;
                    written += u64::try_from(rows.len()).unwrap_or(0);
                    progress(written);
                }
                Some(Ok(ExecEvent::Notice(n))) => self.push_notice(n),
                Some(Ok(other)) => {
                    // The set ended; let the normal path record it and everything after it.
                    {
                        let mut s = self.lock();
                        if let Some(st) = s.sets.get_mut(set) {
                            st.exported_rest = true;
                        }
                    }
                    drop(guard);
                    self.apply(other)?;
                    return Ok(written);
                }
                Some(Err(e)) => {
                    *guard = None;
                    self.finish(Some(e.clone()));
                    return Err(e);
                }
                None => {
                    *guard = None;
                    let mut s = self.lock();
                    if let Some(st) = s.sets.get_mut(set) {
                        st.exported_rest = true;
                        st.ended = true;
                    }
                    s.finished = true;
                    break;
                }
            }
        }
        Ok(written)
    }

    /// Reads one event. Returns false when the stream is over.
    async fn pull_one(&self) -> Result<bool> {
        let mut guard = self.stream.lock().await;
        let Some(stream) = guard.as_mut() else {
            return Ok(false);
        };
        let mut cancel = self.cancel_tx.subscribe();
        let event = tokio::select! {
            e = guarded("Reading results", async { Ok(stream.next().await) }) => e,
            _ = cancel.wait_for(|c| *c) => {
                *guard = None;
                let err = Error::cancelled();
                self.finish(Some(err.clone()));
                return Err(err);
            }
        };
        match event {
            Err(panic) | Ok(Some(Err(panic))) => {
                *guard = None;
                self.finish(Some(panic.clone()));
                Err(panic)
            }
            Ok(None) => {
                *guard = None;
                self.finish(None);
                Ok(false)
            }
            Ok(Some(Ok(event))) => {
                let stop = self.apply(event)?;
                if stop {
                    // The row limit was reached: drop the stream so the server cursor closes.
                    *guard = None;
                    self.finish(None);
                }
                Ok(true)
            }
        }
    }

    /// Records one event. Returns true when reading should stop because of the row limit.
    fn apply(&self, event: ExecEvent) -> Result<bool> {
        let page_rows = self.options.page_rows;
        let mut s = self.lock();
        s.last_event = Some(Instant::now());
        match event {
            ExecEvent::ResultStart(columns) => {
                // A new set implicitly ends the previous one.
                if let Some(last) = s.sets.last_mut() {
                    last.ended = true;
                }
                s.sets.push(SetState::new(columns));
            }
            ExecEvent::Rows(rows) => {
                let set_index = s.sets.len().saturating_sub(1);
                if s.sets.is_empty() {
                    s.sets.push(SetState::new(Vec::new()));
                }
                let max = self.options.max_rows;
                let st = s
                    .sets
                    .last_mut()
                    .ok_or_else(|| Error::internal("rows without a result set"))?;
                let mut stop = false;
                for row in rows {
                    if max.is_some_and(|m| st.rows >= m) {
                        st.limited = true;
                        st.ended = true;
                        stop = true;
                        break;
                    }
                    st.tail.push(row);
                    st.rows += 1;
                    if st.tail.len() >= page_rows {
                        let page = std::mem::take(&mut st.tail);
                        self.store.insert(
                            PageKey {
                                result: self.id,
                                set: u32::try_from(set_index).unwrap_or(u32::MAX),
                                page: st.full_pages,
                            },
                            page,
                        )?;
                        st.full_pages += 1;
                    }
                }
                if !stop && max.is_some_and(|m| st.rows >= m) {
                    // Exactly at the limit: more rows may exist, so stop here as well.
                    st.limited = true;
                    st.ended = true;
                    stop = true;
                }
                return Ok(stop);
            }
            ExecEvent::ResultEnd {
                affected,
                truncated,
            } => {
                let needs_new = s.sets.last().is_none_or(|st| st.ended);
                if needs_new {
                    s.sets.push(SetState::new(Vec::new()));
                }
                if let Some(st) = s.sets.last_mut() {
                    st.ended = true;
                    st.affected = affected;
                    st.limited |= truncated;
                }
            }
            ExecEvent::Notice(n) => {
                drop(s);
                self.push_notice(n);
            }
        }
        Ok(false)
    }

    fn push_notice(&self, n: Notice) {
        let mut s = self.lock();
        if s.notices.len() < MAX_NOTICES {
            s.notices.push(n);
        } else {
            s.dropped_notices += 1;
        }
    }

    fn finish(&self, error: Option<Error>) {
        let mut s = self.lock();
        s.finished = true;
        s.last_event = Some(Instant::now());
        if let Some(last) = s.sets.last_mut() {
            last.ended = true;
        }
        if error.is_some() && s.error.is_none() {
            s.error = error;
        }
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        self.store.release(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use crate::cache::DEFAULT_BUDGET;
    use crate::error::ErrorKind;
    use crate::exec::ValueKind;
    use futures::stream;

    fn store() -> (tempfile::TempDir, Arc<PageStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(DEFAULT_BUDGET, dir.path().into()).unwrap();
        (dir, store)
    }

    fn col(name: &str) -> Column {
        Column::new(name, "int8", ValueKind::Integer)
    }

    fn rows(from: i64, n: i64) -> ExecEvent {
        ExecEvent::Rows((from..from + n).map(|i| vec![Value::I64(i)]).collect())
    }

    fn opts(page_rows: usize) -> ExecutionOptions {
        ExecutionOptions {
            page_rows,
            max_rows: None,
        }
    }

    #[tokio::test]
    async fn windows_span_pages_and_the_tail() {
        let (_d, store) = store();
        let events = vec![
            Ok(ExecEvent::ResultStart(vec![col("n")])),
            Ok(rows(0, 7)),
            Ok(rows(7, 6)),
            Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            }),
        ];
        let ex = Execution::new(Box::pin(stream::iter(events)), store, opts(5));
        let w = ex.window(0, 3, 6).await.unwrap();
        let got: Vec<Value> = w.rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(got, (3..9).map(Value::I64).collect::<Vec<_>>());
        let all = ex.window(0, 0, 100).await.unwrap();
        assert_eq!(all.rows.len(), 13);
        let st = ex.status();
        assert!(st.finished || st.sets[0].complete);
        assert_eq!(st.sets[0].rows, 13);
    }

    #[tokio::test]
    async fn fetching_stops_where_the_reader_stops() {
        let (_d, store) = store();
        let pulled = Arc::new(AtomicU64::new(0));
        let p = pulled.clone();
        let s = stream::iter(0..)
            .map(move |i| {
                p.fetch_add(1, Ordering::SeqCst);
                if i == 0 {
                    Ok(ExecEvent::ResultStart(vec![col("n")]))
                } else {
                    Ok(rows(i * 10, 10))
                }
            })
            .boxed();
        let ex = Execution::new(s, store, opts(10));
        ex.prime().await.unwrap();
        let w = ex.window(0, 0, 25).await.unwrap();
        assert_eq!(w.rows.len(), 25);
        // One start event and three batches of ten: nothing more was read.
        assert_eq!(pulled.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn the_row_limit_stops_and_marks_the_set() {
        let (_d, store) = store();
        let s = stream::iter(0..)
            .map(|i| {
                if i == 0 {
                    Ok(ExecEvent::ResultStart(vec![col("n")]))
                } else {
                    Ok(rows(i, 400))
                }
            })
            .boxed();
        let ex = Execution::new(
            s,
            store,
            ExecutionOptions {
                page_rows: 100,
                max_rows: Some(1_000),
            },
        );
        let w = ex.window(0, 0, 5_000).await.unwrap();
        assert_eq!(w.rows.len(), 1_000);
        let st = ex.status();
        assert!(st.sets[0].limited);
        assert!(!st.sets[0].complete);
        assert!(st.finished);
    }

    #[tokio::test]
    async fn several_result_sets_and_affected_counts() {
        let (_d, store) = store();
        let events = vec![
            Ok(ExecEvent::ResultEnd {
                affected: Some(3),
                truncated: false,
            }),
            Ok(ExecEvent::ResultStart(vec![col("a")])),
            Ok(rows(0, 2)),
            Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            }),
            Ok(ExecEvent::Notice(Notice {
                level: crate::exec::NoticeLevel::Info,
                message: "done".into(),
                code: None,
            })),
            Ok(ExecEvent::ResultStart(vec![col("b")])),
            Ok(rows(10, 1)),
            Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            }),
        ];
        let ex = Execution::new(Box::pin(stream::iter(events)), store, opts(50));
        ex.prime().await.unwrap();
        let st = ex.status();
        assert_eq!(st.sets.len(), 3);
        assert_eq!(st.sets[0].affected, Some(3));
        assert!(st.sets[0].columns.is_empty());
        assert_eq!(st.sets[1].rows, 2);
        assert_eq!(st.sets[2].rows, 1);
        assert_eq!(st.notices.len(), 1);
        let w = ex.window(2, 0, 10).await.unwrap();
        assert_eq!(w.rows, vec![vec![Value::I64(10)]]);
    }

    #[tokio::test]
    async fn errors_end_the_execution() {
        let (_d, store) = store();
        let events = vec![
            Ok(ExecEvent::ResultStart(vec![col("a")])),
            Ok(rows(0, 3)),
            Err(Error::new(ErrorKind::Server, "division by zero")),
        ];
        let ex = Execution::new(Box::pin(stream::iter(events)), store, opts(50));
        let err = ex.window(0, 0, 10).await.unwrap_err();
        assert_eq!(err.message, "division by zero");
        let st = ex.status();
        assert!(st.finished);
        assert_eq!(st.sets[0].rows, 3);
        assert!(st.error.is_some());
    }

    #[tokio::test]
    async fn cancel_interrupts_a_waiting_fetch() {
        let (_d, store) = store();
        let s = stream::iter(vec![Ok(ExecEvent::ResultStart(vec![col("a")]))])
            .chain(stream::pending())
            .boxed();
        let ex = Arc::new(Execution::new(s, store, opts(10)));
        let reader = {
            let ex = ex.clone();
            tokio::spawn(async move { ex.window(0, 0, 10).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ex.cancel();
        let err = reader.await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert!(ex.status().finished);
    }

    #[tokio::test]
    async fn a_panicking_driver_stream_becomes_an_error() {
        let (_d, store) = store();
        let s = stream::iter(0..2)
            .map(|i| {
                if i == 1 {
                    panic!("decoder bug");
                }
                Ok(ExecEvent::ResultStart(vec![col("a")]))
            })
            .boxed();
        let ex = Execution::new(s, store, opts(10));
        let err = ex.window(0, 0, 10).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Internal);
    }

    struct Collect(Vec<Row>, usize);
    impl RowSink for Collect {
        fn begin(&mut self, columns: &[Column]) -> Result<()> {
            self.1 = columns.len();
            Ok(())
        }
        fn rows(&mut self, rows: &[Row]) -> Result<()> {
            self.0.extend_from_slice(rows);
            Ok(())
        }
    }

    #[tokio::test]
    async fn drain_writes_cached_rows_then_streams_the_rest() {
        let (_d, store) = store();
        let events = std::iter::once(Ok(ExecEvent::ResultStart(vec![col("n")])))
            .chain((0..20).map(|b| Ok(rows(b * 50, 50))))
            .chain(std::iter::once(Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            })));
        let ex = Execution::new(Box::pin(stream::iter(events)), store.clone(), opts(100));
        ex.window(0, 0, 150).await.unwrap();
        let kept_before = ex.status().sets[0].rows;
        let mut sink = Collect(Vec::new(), 0);
        let last = AtomicU64::new(0);
        let n = ex
            .drain(0, &mut sink, &|w| last.store(w, Ordering::SeqCst))
            .await
            .unwrap();
        assert_eq!(n, 1_000);
        assert_eq!(sink.1, 1);
        assert_eq!(sink.0.len(), 1_000);
        assert_eq!(sink.0[999], vec![Value::I64(999)]);
        assert_eq!(last.load(Ordering::SeqCst), 1_000);
        let st = ex.status();
        assert!(st.sets[0].exported_rest);
        assert_eq!(st.sets[0].rows, kept_before);
    }

    #[tokio::test]
    async fn closing_releases_cached_pages() {
        let (_d, store) = store();
        let events = vec![Ok(ExecEvent::ResultStart(vec![col("a")])), Ok(rows(0, 30))];
        let ex = Execution::new(Box::pin(stream::iter(events)), store.clone(), opts(10));
        ex.window(0, 0, 30).await.unwrap();
        assert!(store.stats().memory_pages >= 3);
        drop(ex);
        assert_eq!(store.stats().memory_pages, 0);
    }
}
