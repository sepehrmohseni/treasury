//! The traits a driver implements. A driver is a set of capabilities rather than one large
//! interface: every session can connect, ping and close; the rest is optional, reported in
//! [`Capabilities`], and the UI only offers what a session supports.

use crate::analyze::Analyzer;
use crate::config::{ConnectionConfig, Engine, SecretSlot};
use crate::edit::{Changeset, Expiry, TableRef};
use crate::error::Result;
use crate::exec::{ExecRequest, ExecStream};
use crate::health::{Metric, MetricSet};
use crate::plan::{ExplainMode, Plan};
use crate::schema::{ChildPage, ChildQuery, ObjectDetails, ObjectPath};
use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// The SQL dialects, which decide parsing, quoting and limit syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlDialect {
    /// PostgreSQL.
    Postgres,
    /// MySQL.
    MySql,
    /// MariaDB.
    MariaDb,
    /// T-SQL.
    MsSql,
    /// Snowflake SQL.
    Snowflake,
    /// Databricks SQL.
    Databricks,
    /// Oracle SQL and PL/SQL.
    Oracle,
    /// Db2 SQL.
    Db2,
}

/// The language a session's editor speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "dialect", rename_all = "snake_case")]
pub enum QueryLanguage {
    /// SQL in a dialect.
    Sql(SqlDialect),
    /// MongoDB commands as Extended JSON.
    Mongo,
    /// Redis commands.
    Redis,
    /// CQL.
    Cql,
}

/// How a running statement is stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMethod {
    /// The protocol's own cancel request (PostgreSQL cancel key, TDS attention, HTTP cancel).
    Native,
    /// A second connection kills the query (MySQL `KILL QUERY`, MongoDB `killOp`).
    KillFromSecondConnection,
    /// The client stops waiting and closes the request; the server may finish the work.
    ClientOnly,
}

/// Who enforces read-only sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyEnforcement {
    /// The server refuses writes; Treasury blocks only attempts to switch that off.
    Server,
    /// Only Treasury's classifier stands between the user and a write; unknown statements are
    /// blocked.
    Client,
}

/// What a session supports. The UI reads this and hides everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The editor language.
    pub language: QueryLanguage,
    /// Lazy object tree.
    pub introspect: bool,
    /// DDL for objects.
    pub ddl: bool,
    /// Plans: `None`, or whether analyze mode and a structured tree are available.
    pub explain: Option<ExplainSupport>,
    /// How cancel works.
    pub cancel: CancelMethod,
    /// Overview metrics.
    pub health: bool,
    /// Row editing.
    pub edit: bool,
    /// Explicit transactions.
    pub transactions: bool,
    /// Who enforces read-only.
    pub read_only: ReadOnlyEnforcement,
    /// One statement can return several result sets.
    pub multiple_results: bool,
}

/// Plan support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainSupport {
    /// Actual-run plans exist.
    pub analyze: bool,
}

/// Facts about the server, shown in the status bar and the overview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Product name as the server reports it.
    pub product: String,
    /// Version string.
    pub version: String,
    /// Other facts: current user, database, region, warehouse, cluster mode.
    pub details: Vec<(String, String)>,
}

/// Secrets a driver may read while connecting. The SecretStore implements it; tests use a map.
pub trait SecretSource: Send + Sync {
    /// The secret in `slot`, if set.
    fn secret(&self, slot: SecretSlot) -> Result<Option<SecretString>>;
}

/// What the core gives a driver besides the saved settings.
pub struct ConnectContext {
    /// Secrets for this connection.
    pub secrets: Arc<dyn SecretSource>,
    /// Connect through this local address instead of the configured host (an SSH tunnel). TLS
    /// still verifies the configured host name.
    pub via: Option<std::net::SocketAddr>,
    /// Time limit for establishing the connection.
    pub connect_timeout: Duration,
    /// Time limit for the metadata queries Treasury issues by itself.
    pub metadata_timeout: Duration,
    /// Start the session read-only.
    pub read_only: bool,
}

/// An engine's driver: the factory for sessions.
#[async_trait]
pub trait Driver: Send + Sync {
    /// The engine it speaks to.
    fn engine(&self) -> Engine;

    /// Opens a session.
    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>>;
}

/// One open connection to a server.
#[async_trait]
pub trait Session: Send + Sync {
    /// What this session supports.
    fn capabilities(&self) -> Capabilities;

    /// Statement analysis rules for this session's language.
    fn analyzer(&self) -> &dyn Analyzer;

    /// Round-trips to the server and reports how long it took.
    async fn ping(&self) -> Result<Duration>;

    /// Product, version and other facts.
    async fn server_info(&self) -> Result<ServerInfo>;

    /// Turns server-enforced read-only on or off where the server has it. Sessions with
    /// [`ReadOnlyEnforcement::Client`] accept the call and change nothing.
    async fn set_read_only(&self, read_only: bool) -> Result<()>;

    /// Closes the connection. Further calls fail.
    async fn close(&self) -> Result<()>;

    /// Running statements.
    fn execute(&self) -> Option<&dyn Execute> {
        None
    }

    /// Cancelling the running statement.
    fn cancel(&self) -> Option<&dyn Cancel> {
        None
    }

    /// Browsing objects.
    fn introspect(&self) -> Option<&dyn Introspect> {
        None
    }

    /// Plans.
    fn explain(&self) -> Option<&dyn Explain> {
        None
    }

    /// Overview metrics.
    fn health(&self) -> Option<&dyn Health> {
        None
    }

    /// Row editing.
    fn edit(&self) -> Option<&dyn Edit> {
        None
    }
}

/// Running statements.
#[async_trait]
pub trait Execute: Send + Sync {
    /// Starts `request` and returns its events. Only one statement runs per session at a time;
    /// the core serializes calls.
    async fn execute(&self, request: ExecRequest) -> Result<ExecStream>;
}

/// Cancelling.
#[async_trait]
pub trait Cancel: Send + Sync {
    /// Stops the statement currently running on this session, if any.
    async fn cancel(&self) -> Result<()>;
}

/// The object tree.
#[async_trait]
pub trait Introspect: Send + Sync {
    /// Children of `parent`, one page at a time.
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage>;

    /// Details of one object.
    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails>;

    /// The object's DDL, when the engine can produce it.
    async fn ddl(&self, path: &ObjectPath) -> Result<Option<String>>;
}

/// Plans.
#[async_trait]
pub trait Explain: Send + Sync {
    /// The plan of `statement`. The core only asks for [`ExplainMode::Analyze`] on reads.
    async fn explain(&self, statement: &str, mode: ExplainMode) -> Result<Plan>;
}

/// Overview data.
#[async_trait]
pub trait Health: Send + Sync {
    /// Reads `set`. Metrics that cannot be read come back with their reason, not as errors.
    async fn metrics(&self, set: MetricSet) -> Result<Vec<Metric>>;
}

/// Row editing.
#[async_trait]
pub trait Edit: Send + Sync {
    /// The statements `changes` would run, with values rendered for review.
    fn render(&self, changes: &Changeset) -> Result<Vec<String>>;

    /// Applies `changes` in one transaction and returns the rows affected.
    async fn apply(&self, changes: &Changeset) -> Result<u64>;

    /// The columns that identify one row of `table`: its primary key, or else a unique key
    /// whose columns are all NOT NULL. Empty when it has neither. An unqualified name is
    /// resolved the way the server resolves it for this session.
    async fn row_key(&self, table: &TableRef) -> Result<Vec<String>>;

    /// When the rows' source expires, for engines that expire whole keys; `None` when it has no
    /// expiry to show or change.
    async fn expiry(&self, _table: &TableRef) -> Result<Option<Expiry>> {
        Ok(None)
    }
}
