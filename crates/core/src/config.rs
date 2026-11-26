//! Connection settings. Secrets are never part of a [`ConnectionConfig`]: they live in the
//! SecretStore under a [`SecretSlot`] of the connection, and the rest of the app only learns
//! whether a slot is set.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The database engines Treasury connects to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// PostgreSQL.
    Postgres,
    /// MySQL.
    MySql,
    /// MariaDB.
    MariaDb,
    /// Microsoft SQL Server.
    SqlServer,
    /// MongoDB.
    MongoDb,
    /// Redis.
    Redis,
    /// Valkey.
    Valkey,
    /// Apache Cassandra.
    Cassandra,
    /// ScyllaDB.
    ScyllaDb,
    /// Snowflake.
    Snowflake,
    /// Databricks SQL.
    Databricks,
    /// Oracle Database.
    Oracle,
    /// IBM Db2.
    Db2,
}

/// How an engine organizes data, which decides the editor and the result views.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Tables and SQL.
    Relational,
    /// Collections of documents.
    Document,
    /// Keys and typed values.
    KeyValue,
    /// Partitioned wide-column tables and CQL.
    WideColumn,
}

impl Engine {
    /// Every engine, in the order the UI lists them.
    pub const ALL: [Engine; 13] = [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::SqlServer,
        Engine::MongoDb,
        Engine::Redis,
        Engine::Valkey,
        Engine::Cassandra,
        Engine::ScyllaDb,
        Engine::Snowflake,
        Engine::Databricks,
        Engine::Oracle,
        Engine::Db2,
    ];

    /// The product name.
    pub fn display_name(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
            Engine::MariaDb => "MariaDB",
            Engine::SqlServer => "SQL Server",
            Engine::MongoDb => "MongoDB",
            Engine::Redis => "Redis",
            Engine::Valkey => "Valkey",
            Engine::Cassandra => "Cassandra",
            Engine::ScyllaDb => "ScyllaDB",
            Engine::Snowflake => "Snowflake",
            Engine::Databricks => "Databricks SQL",
            Engine::Oracle => "Oracle",
            Engine::Db2 => "Db2",
        }
    }

    /// The port a server listens on unless configured otherwise.
    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::MySql | Engine::MariaDb => 3306,
            Engine::SqlServer => 1433,
            Engine::MongoDb => 27017,
            Engine::Redis | Engine::Valkey => 6379,
            Engine::Cassandra | Engine::ScyllaDb => 9042,
            Engine::Snowflake | Engine::Databricks => 443,
            Engine::Oracle => 1521,
            Engine::Db2 => 50000,
        }
    }

    /// The data model.
    pub fn family(self) -> Family {
        match self {
            Engine::MongoDb => Family::Document,
            Engine::Redis | Engine::Valkey => Family::KeyValue,
            Engine::Cassandra | Engine::ScyllaDb => Family::WideColumn,
            _ => Family::Relational,
        }
    }

    /// Whether the engine runs in a separate bridge process that needs a vendor client
    /// library, which is why it is not available on mobile.
    pub fn needs_bridge(self) -> bool {
        matches!(self, Engine::Oracle | Engine::Db2)
    }

    /// Whether the engine is reached over HTTPS APIs rather than a database wire protocol.
    pub fn is_cloud_warehouse(self) -> bool {
        matches!(self, Engine::Snowflake | Engine::Databricks)
    }
}

/// Where a connection points, which decides how careful Treasury is with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    /// A database on this machine or in a local container.
    Local,
    /// A shared development database.
    #[default]
    Development,
    /// A staging or test database.
    Staging,
    /// Production: opens read-only, confirms destructive statements.
    Production,
}

/// A connection's identifier: a UUID string, stable across renames.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(pub String);

impl ConnectionId {
    /// A new random identifier.
    pub fn random() -> Result<Self> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| Error::internal(format!("no randomness: {e}")))?;
        // Version 4, variant RFC 4122.
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(Self(crate::format::format_uuid(&b)))
    }
}

/// The named secrets a connection can have. Their values live only in the SecretStore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSlot {
    /// The database password (also a Redis ACL password).
    Password,
    /// A token: Databricks personal access token or Snowflake OAuth access token.
    Token,
    /// An OAuth client secret (Databricks machine-to-machine).
    ClientSecret,
    /// An OAuth refresh token kept after a browser sign-in.
    RefreshToken,
    /// Passphrase of a private key file (Snowflake key pair, TLS client key).
    KeyPassphrase,
    /// SSH password for the tunnel host.
    SshPassword,
    /// Passphrase of the SSH key file.
    SshKeyPassphrase,
    /// SSH password for the jump host.
    JumpPassword,
    /// Passphrase of the jump host's SSH key file.
    JumpKeyPassphrase,
}

impl SecretSlot {
    /// The key under which the slot is stored for `connection`.
    pub fn key(self, connection: &ConnectionId) -> String {
        let name = match self {
            SecretSlot::Password => "password",
            SecretSlot::Token => "token",
            SecretSlot::ClientSecret => "client-secret",
            SecretSlot::RefreshToken => "refresh-token",
            SecretSlot::KeyPassphrase => "key-passphrase",
            SecretSlot::SshPassword => "ssh-password",
            SecretSlot::SshKeyPassphrase => "ssh-key-passphrase",
            SecretSlot::JumpPassword => "jump-password",
            SecretSlot::JumpKeyPassphrase => "jump-key-passphrase",
        };
        format!("connection/{}/{name}", connection.0)
    }
}

/// How the server is addressed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Endpoint {
    /// One host and port.
    Host {
        /// Host name or address.
        host: String,
        /// TCP port.
        port: u16,
    },
    /// Several seed hosts (Cassandra, Redis Cluster, Sentinel, MongoDB replica sets).
    Hosts {
        /// Host and port pairs.
        hosts: Vec<(String, u16)>,
    },
    /// A connection string the driver understands natively, with the password removed
    /// (MongoDB `mongodb+srv://` URIs).
    Uri {
        /// The URI without credentials.
        uri: String,
    },
    /// A Snowflake account identifier (`orgname-account` or a locator).
    Account {
        /// The account identifier.
        account: String,
    },
    /// A Databricks workspace and the HTTP path of a SQL warehouse.
    Workspace {
        /// Workspace host, such as `adb-123.4.azuredatabricks.net`.
        host: String,
        /// The warehouse's HTTP path, `/sql/1.0/warehouses/<id>`.
        http_path: String,
    },
}

/// How the client proves who it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethod {
    /// No authentication.
    None,
    /// User and password (SCRAM for PostgreSQL and MongoDB, ACL user for Redis).
    Password,
    /// The TLS client certificate identifies the user (MongoDB X.509, MySQL REQUIRE X509).
    ClientCertificate,
    /// Snowflake key-pair authentication: a JWT signed with the user's private key.
    KeyPair {
        /// PEM file holding the PKCS#8 private key.
        private_key: PathBuf,
    },
    /// An access token (Databricks personal access token, Snowflake OAuth token).
    Token,
    /// OAuth machine-to-machine with a client ID and secret (Databricks service principals).
    OAuthClient {
        /// The client ID.
        client_id: String,
    },
    /// OAuth user sign-in in the system browser with PKCE (Databricks).
    OAuthBrowser {
        /// The client ID registered for Treasury in the workspace.
        client_id: String,
    },
}

/// TLS settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Whether TLS is used.
    pub mode: TlsMode,
    /// Extra trusted CA certificates (PEM), on top of the system's.
    pub ca_file: Option<PathBuf>,
    /// Client certificate chain (PEM).
    pub client_cert: Option<PathBuf>,
    /// Client private key (PEM); its passphrase, if any, is the `KeyPassphrase` slot.
    pub client_key: Option<PathBuf>,
    /// Skip certificate and host name verification. The connection shows a permanent warning.
    pub accept_invalid: bool,
    /// The name to verify instead of the host (for tunnels and IP addresses).
    pub server_name: Option<String>,
}

/// When TLS is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Plain TCP.
    Disable,
    /// TLS when the server offers it, plain otherwise.
    Prefer,
    /// TLS always; verify the certificate.
    #[default]
    Require,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            mode: TlsMode::Require,
            ca_file: None,
            client_cert: None,
            client_key: None,
            accept_invalid: false,
            server_name: None,
        }
    }
}

/// One SSH host on the way to the database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHop {
    /// SSH host.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// SSH user.
    pub user: String,
    /// How to authenticate.
    pub auth: SshAuth,
}

/// SSH authentication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshAuth {
    /// A password (the `SshPassword` or `JumpPassword` slot).
    Password,
    /// A private key file; its passphrase is the matching `...KeyPassphrase` slot.
    KeyFile {
        /// Path to the private key.
        path: PathBuf,
    },
    /// The running SSH agent (`SSH_AUTH_SOCK`, or the OpenSSH agent pipe on Windows).
    Agent,
}

/// An SSH tunnel, optionally through one jump host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    /// The host that forwards to the database.
    pub target: SshHop,
    /// A bastion reached first.
    pub jump: Option<SshHop>,
}

/// Everything about a saved connection except its secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Stable identifier.
    pub id: ConnectionId,
    /// Name shown in the tree and typed to confirm destructive statements.
    pub name: String,
    /// The engine.
    pub engine: Engine,
    /// Where the server is.
    pub endpoint: Endpoint,
    /// Default database, keyspace, catalog or Redis database number.
    pub database: Option<String>,
    /// User name.
    pub user: Option<String>,
    /// Authentication method.
    pub auth: AuthMethod,
    /// Local, development, staging or production.
    pub environment: Environment,
    /// Optional color (`#rrggbb`), always shown together with the environment's name.
    pub color: Option<String>,
    /// Open read-only. Production connections are read-only regardless of this.
    pub read_only: bool,
    /// Folder in the connection tree, `/`-separated.
    pub folder: Option<String>,
    /// TLS settings.
    pub tls: TlsConfig,
    /// SSH tunnel.
    pub ssh: Option<SshConfig>,
    /// Engine-specific settings (Snowflake warehouse and role, Redis mode, MongoDB auth source).
    pub options: BTreeMap<String, String>,
    /// Close after this many seconds idle; reconnect on next use.
    pub idle_timeout_secs: Option<u32>,
    /// Record query history. `None` means the default: on, except for production.
    pub history: Option<bool>,
    /// Allow model requests for this connection. `None` means the default: on, except for
    /// production.
    pub model_use: Option<bool>,
    /// Send column comments to the model provider.
    pub model_comments: bool,
}

impl ConnectionConfig {
    /// A new connection with defaults for `engine`, pointing at `host` on the engine's port.
    pub fn new(name: &str, engine: Engine, host: &str) -> Result<Self> {
        Ok(Self {
            id: ConnectionId::random()?,
            name: name.to_owned(),
            engine,
            endpoint: Endpoint::Host {
                host: host.to_owned(),
                port: engine.default_port(),
            },
            database: None,
            user: None,
            auth: AuthMethod::Password,
            environment: Environment::default(),
            color: None,
            read_only: false,
            folder: None,
            tls: TlsConfig::default(),
            ssh: None,
            options: BTreeMap::new(),
            idle_timeout_secs: None,
            history: None,
            model_use: None,
            model_comments: false,
        })
    }

    /// Whether sessions on this connection start read-only.
    pub fn opens_read_only(&self) -> bool {
        self.read_only || self.environment == Environment::Production
    }

    /// Whether query history is recorded.
    pub fn records_history(&self) -> bool {
        self.history
            .unwrap_or(self.environment != Environment::Production)
    }

    /// Whether model requests are allowed.
    pub fn allows_model(&self) -> bool {
        self.model_use
            .unwrap_or(self.environment != Environment::Production)
    }

    /// The host and port of the first server, for messages and tunnels.
    pub fn primary_host(&self) -> Option<(&str, u16)> {
        match &self.endpoint {
            Endpoint::Host { host, port } => Some((host.as_str(), *port)),
            Endpoint::Hosts { hosts } => hosts.first().map(|(h, p)| (h.as_str(), *p)),
            Endpoint::Workspace { host, .. } => Some((host.as_str(), 443)),
            Endpoint::Uri { .. } | Endpoint::Account { .. } => None,
        }
    }

    /// The secret slots this configuration reads.
    pub fn secret_slots(&self) -> Vec<SecretSlot> {
        let mut slots = Vec::new();
        match &self.auth {
            AuthMethod::Password => slots.push(SecretSlot::Password),
            AuthMethod::KeyPair { .. } => slots.push(SecretSlot::KeyPassphrase),
            AuthMethod::Token => slots.push(SecretSlot::Token),
            AuthMethod::OAuthClient { .. } => slots.push(SecretSlot::ClientSecret),
            AuthMethod::OAuthBrowser { .. } => slots.push(SecretSlot::RefreshToken),
            AuthMethod::None | AuthMethod::ClientCertificate => {}
        }
        if self.tls.client_key.is_some() && !slots.contains(&SecretSlot::KeyPassphrase) {
            slots.push(SecretSlot::KeyPassphrase);
        }
        if let Some(ssh) = &self.ssh {
            slots.push(match ssh.target.auth {
                SshAuth::Password => SecretSlot::SshPassword,
                SshAuth::KeyFile { .. } | SshAuth::Agent => SecretSlot::SshKeyPassphrase,
            });
            if let Some(jump) = &ssh.jump {
                slots.push(match jump.auth {
                    SshAuth::Password => SecretSlot::JumpPassword,
                    SshAuth::KeyFile { .. } | SshAuth::Agent => SecretSlot::JumpKeyPassphrase,
                });
            }
        }
        slots
    }

    /// Checks the settings for mistakes the user can fix in the form.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::config(
                Some("name"),
                "Enter a name for the connection.",
            ));
        }
        if self.name.chars().count() > 200 {
            return Err(Error::config(
                Some("name"),
                "Use a name of at most 200 characters.",
            ));
        }
        match &self.endpoint {
            Endpoint::Host { host, port } => {
                check_host(host)?;
                check_port(*port)?;
            }
            Endpoint::Hosts { hosts } => {
                if hosts.is_empty() {
                    return Err(Error::config(Some("host"), "Enter at least one host."));
                }
                for (h, p) in hosts {
                    check_host(h)?;
                    check_port(*p)?;
                }
            }
            Endpoint::Uri { uri } => {
                if uri.trim().is_empty() {
                    return Err(Error::config(Some("uri"), "Enter a connection string."));
                }
            }
            Endpoint::Account { account } => {
                if account.trim().is_empty() {
                    return Err(Error::config(
                        Some("account"),
                        "Enter the account identifier.",
                    ));
                }
            }
            Endpoint::Workspace { host, http_path } => {
                check_host(host)?;
                if !http_path.starts_with('/') {
                    return Err(Error::config(
                        Some("http_path"),
                        "Enter the warehouse's HTTP path, which starts with /sql/.",
                    ));
                }
            }
        }
        if let Some(color) = &self.color {
            let ok = color.len() == 7
                && color.starts_with('#')
                && color[1..].chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(Error::config(
                    Some("color"),
                    "Use a color in the form #1e5640.",
                ));
            }
        }
        self.check_auth()?;
        if let Some(ssh) = &self.ssh {
            check_hop(&ssh.target, "ssh")?;
            if let Some(jump) = &ssh.jump {
                check_hop(jump, "jump")?;
            }
        }
        if self.tls.client_key.is_some() != self.tls.client_cert.is_some() {
            return Err(Error::config(
                Some("tls"),
                "A client certificate needs both the certificate and the key file.",
            ));
        }
        Ok(())
    }

    fn check_auth(&self) -> Result<()> {
        use AuthMethod as A;
        use Engine as E;
        let ok = match (&self.auth, self.engine) {
            (A::None, E::Redis | E::Valkey | E::MongoDb | E::Cassandra | E::ScyllaDb) => true,
            (A::Password, E::Snowflake | E::Databricks) => self.engine == E::Snowflake,
            (A::Password, _) => true,
            (A::ClientCertificate, E::MongoDb | E::MySql | E::MariaDb | E::Postgres) => true,
            (A::KeyPair { .. }, E::Snowflake) => true,
            (A::Token, E::Snowflake | E::Databricks) => true,
            (A::OAuthClient { .. } | A::OAuthBrowser { .. }, E::Databricks) => true,
            _ => false,
        };
        if !ok {
            return Err(Error::config(
                Some("auth"),
                format!(
                    "{} does not support this authentication method.",
                    self.engine.display_name()
                ),
            ));
        }
        if matches!(self.auth, A::ClientCertificate) && self.tls.client_cert.is_none() {
            return Err(Error::config(
                Some("tls"),
                "Certificate authentication needs a client certificate and key.",
            ));
        }
        Ok(())
    }
}

fn check_host(host: &str) -> Result<()> {
    let h = host.trim();
    if h.is_empty() {
        return Err(Error::config(Some("host"), "Enter a host name or address."));
    }
    if h.len() > 253 || h.chars().any(|c| c.is_whitespace() || c == '/' || c == '@') {
        return Err(Error::config(
            Some("host"),
            format!("\"{h}\" is not a valid host name or address."),
        ));
    }
    Ok(())
}

fn check_port(port: u16) -> Result<()> {
    if port == 0 {
        return Err(Error::config(
            Some("port"),
            "Use a port between 1 and 65535.",
        ));
    }
    Ok(())
}

fn check_hop(hop: &SshHop, field: &str) -> Result<()> {
    check_host(&hop.host).map_err(|_| {
        Error::config(
            Some(field),
            format!("\"{}\" is not a valid SSH host.", hop.host),
        )
    })?;
    check_port(hop.port)?;
    if hop.user.trim().is_empty() {
        return Err(Error::config(Some(field), "Enter the SSH user."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_are_careful() {
        let mut c = ConnectionConfig::new("prod", Engine::Postgres, "db").unwrap();
        assert!(!c.opens_read_only());
        assert!(c.records_history());
        assert!(c.allows_model());
        c.environment = Environment::Production;
        assert!(c.opens_read_only());
        assert!(!c.records_history());
        assert!(!c.allows_model());
        c.history = Some(true);
        assert!(c.records_history());
    }

    #[test]
    fn validation_points_at_the_field() {
        let mut c = ConnectionConfig::new(" ", Engine::MySql, "db").unwrap();
        let err = c.validate().unwrap_err();
        assert_eq!(
            err.kind,
            crate::error::ErrorKind::Config {
                field: Some("name".into())
            }
        );
        c.name = "app".into();
        c.color = Some("green".into());
        assert!(c.validate().is_err());
        c.color = Some("#1e5640".into());
        c.validate().unwrap();
        c.endpoint = Endpoint::Host {
            host: "db".into(),
            port: 0,
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn auth_must_fit_the_engine() {
        let mut c = ConnectionConfig::new("w", Engine::Databricks, "x").unwrap();
        c.endpoint = Endpoint::Workspace {
            host: "adb-1.azuredatabricks.net".into(),
            http_path: "/sql/1.0/warehouses/abc".into(),
        };
        c.auth = AuthMethod::Password;
        assert!(c.validate().is_err());
        c.auth = AuthMethod::Token;
        c.validate().unwrap();
        assert_eq!(c.secret_slots(), vec![SecretSlot::Token]);
    }

    #[test]
    fn ids_are_v4_uuids() {
        let id = ConnectionId::random().unwrap();
        assert_eq!(id.0.len(), 36);
        assert_eq!(&id.0[14..15], "4");
        assert_ne!(id, ConnectionId::random().unwrap());
    }

    #[test]
    fn secret_keys_are_scoped_to_the_connection() {
        let id = ConnectionId("abc".into());
        assert_eq!(
            SecretSlot::SshKeyPassphrase.key(&id),
            "connection/abc/ssh-key-passphrase"
        );
    }
}
