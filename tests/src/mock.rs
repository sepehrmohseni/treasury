//! An in-memory engine for testing the conformance suite and the core runtime without a server.
//!
//! It understands a few commands: `rows N` (N integers counting from 1, produced lazily),
//! `sleep S` (cancellable), `syntax` (a syntax error at position 3), `write` (refused while
//! read-only), `denied` (a permission error) and `types` (one row of assorted values).

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use treasury_core::analyze::{
    Analyzer, Classified, CountPlan, Flags, LimitPlan, Placeholder, StatementClass,
};
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, SecretSlot};
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, ConnectContext, Driver, Execute, Introspect, QueryLanguage,
    ReadOnlyEnforcement, ServerInfo, Session, SqlDialect,
};
use treasury_core::error::ServerMessage;
use treasury_core::exec::{Column, ExecEvent, ExecRequest, ExecStream, ValueKind};
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, ObjectDetails, ObjectKind, ObjectNode, ObjectPath,
};
use treasury_core::value::{Date, Value};
use treasury_core::{Error, ErrorKind, Result};

/// The password the mock accepts.
pub const PASSWORD: &str = "right";

/// The "server": shared by every session of one driver, so a restart breaks them all.
#[derive(Default)]
pub struct MockServer {
    generation: AtomicU64,
}

/// The mock driver.
#[derive(Default, Clone)]
pub struct MockDriver {
    server: Arc<MockServer>,
}

impl MockDriver {
    /// Simulates a server restart: every open session loses its connection.
    pub fn restart(&self) {
        self.server.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Driver for MockDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>> {
        if let Endpoint::Host { host, port: 1 } = &config.endpoint {
            return Err(Error::network(
                host,
                1,
                &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            ));
        }
        let pw = ctx.secrets.secret(SecretSlot::Password)?;
        if pw
            .as_ref()
            .map(|p| secrecy::ExposeSecret::expose_secret(p).to_owned())
            .as_deref()
            != Some(PASSWORD)
        {
            return Err(Error::new(
                ErrorKind::Auth,
                "password authentication failed",
            ));
        }
        Ok(Arc::new(MockSession {
            server: self.server.clone(),
            generation: self.server.generation.load(Ordering::SeqCst),
            closed: AtomicBool::new(false),
            read_only: AtomicBool::new(ctx.read_only),
            cancel: Arc::new(Notify::new()),
        }))
    }
}

struct MockSession {
    server: Arc<MockServer>,
    generation: u64,
    closed: AtomicBool,
    read_only: AtomicBool,
    cancel: Arc<Notify>,
}

impl MockSession {
    fn alive(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst)
            || self.server.generation.load(Ordering::SeqCst) != self.generation
        {
            return Err(Error::network(
                "mock",
                5432,
                &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            ));
        }
        Ok(())
    }
}

struct MockAnalyzer;

impl Analyzer for MockAnalyzer {
    fn classify(&self, script: &str) -> Vec<Classified> {
        let class = if script.trim().starts_with("write") {
            StatementClass::Write
        } else {
            StatementClass::Read
        };
        vec![Classified {
            range: 0..script.len(),
            class,
            flags: Flags::default(),
            summary: script.split_whitespace().next().unwrap_or("").to_owned(),
            reason: None,
        }]
    }
    fn limit(&self, _: &str, _: u64) -> LimitPlan {
        LimitPlan::StopFetching
    }
    fn count(&self, _: &str) -> CountPlan {
        CountPlan::NotApplicable
    }
    fn placeholders(&self, _: &str) -> Vec<Placeholder> {
        Vec::new()
    }
}

#[async_trait]
impl Session for MockSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Sql(SqlDialect::Postgres),
            introspect: true,
            ddl: false,
            explain: None,
            cancel: CancelMethod::Native,
            health: false,
            edit: false,
            transactions: false,
            read_only: ReadOnlyEnforcement::Server,
            multiple_results: false,
        }
    }

    fn analyzer(&self) -> &dyn Analyzer {
        &MockAnalyzer
    }

    async fn ping(&self) -> Result<Duration> {
        self.alive()?;
        Ok(Duration::from_micros(50))
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        self.alive()?;
        Ok(ServerInfo {
            product: "Mock".into(),
            version: "1.0".into(),
            details: Vec::new(),
        })
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        self.alive()?;
        self.read_only.store(read_only, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn execute(&self) -> Option<&dyn Execute> {
        Some(self)
    }

    fn cancel(&self) -> Option<&dyn Cancel> {
        Some(self)
    }

    fn introspect(&self) -> Option<&dyn Introspect> {
        Some(self)
    }
}

fn int_col(name: &str) -> Column {
    Column::new(name, "int8", ValueKind::Integer)
}

fn fail(e: Error) -> ExecStream {
    stream::once(async move { Err(e) }).boxed()
}

#[async_trait]
impl Execute for MockSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        let mut words = req.text.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let arg: u64 = words.next().and_then(|w| w.parse().ok()).unwrap_or(0);
        Ok(match cmd {
            "rows" => {
                let start = stream::iter([Ok(ExecEvent::ResultStart(vec![int_col("n")]))]);
                let batches = stream::unfold(1u64, move |next| async move {
                    if next > arg {
                        return None;
                    }
                    let end = (next + 999).min(arg);
                    let rows = (next..=end)
                        .map(|i| vec![Value::I64(i64::try_from(i).unwrap_or(i64::MAX))])
                        .collect();
                    Some((Ok(ExecEvent::Rows(rows)), end + 1))
                });
                let end = stream::iter([Ok(ExecEvent::ResultEnd {
                    affected: None,
                    truncated: false,
                })]);
                start.chain(batches).chain(end).boxed()
            }
            "sleep" => {
                let wait = Duration::from_secs(arg);
                let limit = req.timeout;
                let cancel = self.cancel.clone();
                let outcome = async move {
                    tokio::select! {
                        () = tokio::time::sleep(wait) => Ok(ExecEvent::ResultEnd { affected: None, truncated: false }),
                        () = cancel.notified() => Err(Error::cancelled()),
                        () = tokio::time::sleep(limit.unwrap_or(Duration::MAX / 4)), if limit.is_some() => {
                            Err(Error::timeout(limit.unwrap_or_default()))
                        }
                    }
                };
                stream::once(outcome).boxed()
            }
            "syntax" => fail(Error::new(ErrorKind::Syntax, "syntax error").with_server(
                ServerMessage {
                    code: Some("42601".into()),
                    message: "syntax error at or near \"x\"".into(),
                    position: Some(3),
                },
            )),
            "write" => {
                if self.read_only.load(Ordering::SeqCst) {
                    fail(Error::new(
                        ErrorKind::ReadOnly,
                        "cannot execute in a read-only transaction",
                    ))
                } else {
                    stream::iter([Ok(ExecEvent::ResultEnd {
                        affected: Some(1),
                        truncated: false,
                    })])
                    .boxed()
                }
            }
            "denied" => fail(Error::new(
                ErrorKind::Permission {
                    grant: Some("SELECT".into()),
                },
                "permission denied",
            )),
            "types" => stream::iter([
                Ok(ExecEvent::ResultStart(vec![
                    Column::new("b", "bool", ValueKind::Bool),
                    int_col("i"),
                    Column::new("t", "text", ValueKind::Text),
                    Column::new("d", "date", ValueKind::Date),
                ])),
                Ok(ExecEvent::Rows(vec![vec![
                    Value::Bool(true),
                    Value::I64(-42),
                    Value::Text("héllo".into()),
                    Value::Date(Date { days: 19_782 }),
                ]])),
                Ok(ExecEvent::ResultEnd {
                    affected: None,
                    truncated: false,
                }),
            ])
            .boxed(),
            "setup" => stream::iter([Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            })])
            .boxed(),
            _ => fail(Error::new(
                ErrorKind::Syntax,
                format!("unknown command {cmd}"),
            )),
        })
    }
}

#[async_trait]
impl Cancel for MockSession {
    async fn cancel(&self) -> Result<()> {
        self.cancel.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl Introspect for MockSession {
    async fn children(&self, parent: &ObjectPath, _: &ChildQuery) -> Result<ChildPage> {
        self.alive()?;
        let (kind, name) = match parent.0.len() {
            0 => (ObjectKind::Database, "db"),
            1 => (ObjectKind::Schema, "main"),
            2 => (ObjectKind::Table, "types"),
            _ => return Ok(ChildPage::default()),
        };
        Ok(ChildPage {
            nodes: vec![ObjectNode {
                path: parent.child(kind, name),
                kind,
                name: name.into(),
                expandable: kind != ObjectKind::Table,
                detail: None,
            }],
            next_cursor: None,
        })
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        self.alive()?;
        if path.leaf().map(|p| p.kind) != Some(ObjectKind::Table) {
            return Err(Error::unsupported("details of non-tables"));
        }
        Ok(ObjectDetails {
            columns: vec![ColumnInfo {
                name: "i".into(),
                type_name: "int8".into(),
                kind: ValueKind::Integer,
                nullable: false,
                default: None,
                comment: None,
                primary_key: true,
            }],
            ..ObjectDetails::default()
        })
    }

    async fn ddl(&self, _: &ObjectPath) -> Result<Option<String>> {
        Ok(None)
    }
}
