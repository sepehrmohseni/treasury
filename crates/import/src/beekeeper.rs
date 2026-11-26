//! Beekeeper Studio: the `saved_connection` table of its `app.db`, with secrets decrypted using
//! the per-install key in the `.key` file next to it.

use crate::crypto::{BEEKEEPER_DEFAULT_KEY, simple_encryptor};
use crate::{Draft, Imported, Report, Skipped, apply_uri, is_local, unreadable};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags};
use secrecy::SecretString;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use treasury_core::Result;
use treasury_core::config::{
    Endpoint, Engine, Environment, SecretSlot, SshAuth, SshConfig, SshHop, TlsMode,
};
use zeroize::Zeroizing;

type Row = HashMap<String, Sql>;

pub fn import(dir: &Path) -> Result<Report> {
    let key_text = std::fs::read_to_string(dir.join(".key"))
        .map_err(|_| unreadable("Beekeeper Studio", "the folder has no .key file"))?;
    let key = simple_encryptor(BEEKEEPER_DEFAULT_KEY, &key_text)
        .and_then(|v| {
            v.get("encryptionKey")
                .and_then(|k| k.as_str())
                .map(str::to_owned)
        })
        .map(Zeroizing::new)
        .ok_or_else(|| unreadable("Beekeeper Studio", "the .key file cannot be read"))?;
    let db = Connection::open_with_flags(dir.join("app.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| unreadable("Beekeeper Studio", e))?;
    let folders = folders(&db);
    let rows = rows(&db).map_err(|e| unreadable("Beekeeper Studio", e))?;
    let mut report = Report::default();
    for row in rows {
        convert(&row, &key, &folders, &mut report);
    }
    Ok(report)
}

fn folders(db: &Connection) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    if let Ok(mut st) = db.prepare("SELECT id, name FROM connection_folder")
        && let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
    {
        for (id, name) in rows.flatten() {
            out.insert(id, name);
        }
    }
    out
}

/// Every column of every row, by name: the table gains columns across Beekeeper versions.
fn rows(db: &Connection) -> rusqlite::Result<Vec<Row>> {
    let mut st = db.prepare("SELECT * FROM saved_connection")?;
    let names: Vec<String> = st.column_names().map(str::to_owned).collect();
    let mapped = st.query_map([], |r| {
        let mut row = Row::new();
        for (i, n) in names.iter().enumerate() {
            row.insert(n.clone(), r.get::<_, Sql>(i)?);
        }
        Ok(row)
    })?;
    mapped.collect()
}

fn text(row: &Row, col: &str) -> Option<String> {
    match row.get(col)? {
        Sql::Text(s) if !s.is_empty() => Some(s.clone()),
        Sql::Integer(n) => Some(n.to_string()),
        _ => None,
    }
}

fn int(row: &Row, col: &str) -> Option<i64> {
    match row.get(col)? {
        Sql::Integer(n) => Some(*n),
        Sql::Text(s) => s.parse().ok(),
        _ => None,
    }
}

fn flag(row: &Row, col: &str) -> bool {
    int(row, col).is_some_and(|n| n != 0)
}

fn secret(row: &Row, col: &str, key: &str) -> Option<SecretString> {
    let sealed = text(row, col)?;
    let value = simple_encryptor(key, &sealed)?;
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| SecretString::from(s.to_owned()))
}

fn json(row: &Row, col: &str) -> serde_json::Value {
    text(row, col)
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn engine(kind: &str) -> Option<Engine> {
    Some(match kind {
        "postgresql" => Engine::Postgres,
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        "sqlserver" => Engine::SqlServer,
        "oracle" => Engine::Oracle,
        "cassandra" => Engine::Cassandra,
        "scylladb" => Engine::ScyllaDb,
        "mongodb" => Engine::MongoDb,
        "redis" => Engine::Redis,
        "snowflake" => Engine::Snowflake,
        _ => return None,
    })
}

fn ssh_auth(mode: Option<&str>, keyfile: Option<String>) -> SshAuth {
    match (mode, keyfile) {
        (Some("keyfile"), Some(path)) => SshAuth::KeyFile {
            path: PathBuf::from(path),
        },
        (Some("userpass"), _) => SshAuth::Password,
        _ => SshAuth::Agent,
    }
}

fn convert(row: &Row, key: &str, folders: &HashMap<i64, String>, report: &mut Report) {
    let name = text(row, "name").unwrap_or_else(|| "Beekeeper connection".into());
    let kind = text(row, "connectionType").unwrap_or_default();
    let Some(engine) = engine(&kind) else {
        report.skipped.push(Skipped {
            name,
            reason: format!("Treasury does not support {kind} connections."),
        });
        return;
    };
    let host = text(row, "host");
    let mut d = Draft::new(&name, engine);
    d.port = int(row, "port").and_then(|p| u16::try_from(p).ok());
    d.database = text(row, "defaultDatabase").or_else(|| text(row, "serviceName"));
    d.user = text(row, "username");
    d.read_only = flag(row, "readOnlyMode");
    d.folder = int(row, "connectionFolderId").and_then(|id| folders.get(&id).cloned());
    d.environment = if host.as_deref().is_some_and(is_local) {
        Environment::Local
    } else {
        Environment::Development
    };
    d.host = host;
    let mut config = match d.build() {
        Ok(c) => c,
        Err(e) => {
            report.skipped.push(Skipped {
                name,
                reason: e.message.clone(),
            });
            return;
        }
    };
    let mut secrets = Vec::new();
    let mut notes = Vec::new();
    if let Some(url) = secret(row, "url", key) {
        use secrecy::ExposeSecret;
        if let Some(pw) = apply_uri(&mut config, url.expose_secret()) {
            secrets.push((SecretSlot::Password, pw));
        }
    }
    if engine == Engine::Snowflake {
        let opts = json(row, "snowflakeOptions");
        if let Some(account) = opts.get("account").and_then(|a| a.as_str()) {
            config.endpoint = Endpoint::Account {
                account: account.to_owned(),
            };
        }
        for k in ["warehouse", "role", "schema"] {
            if let Some(v) = opts.get(k).and_then(|v| v.as_str()) {
                config.options.insert(k.into(), v.into());
            }
        }
    }
    if let Some(pw) = secret(row, "password", key) {
        secrets.push((SecretSlot::Password, pw));
    }
    if flag(row, "ssl") {
        config.tls.mode = TlsMode::Require;
        config.tls.ca_file = text(row, "sslCaFile").map(PathBuf::from);
        config.tls.client_cert = text(row, "sslCertFile").map(PathBuf::from);
        config.tls.client_key = text(row, "sslKeyFile").map(PathBuf::from);
        config.tls.accept_invalid =
            row.contains_key("sslRejectUnauthorized") && !flag(row, "sslRejectUnauthorized");
    } else if engine == Engine::SqlServer {
        config.tls.accept_invalid = flag(row, "trustServerCertificate");
    } else {
        config.tls.mode = TlsMode::Prefer;
    }
    if flag(row, "windowsAuthEnabled") {
        notes.push("Windows authentication is not supported; enter a SQL Server login.".into());
    }
    if flag(row, "sshEnabled")
        && let Some(ssh_host) = text(row, "sshHost")
    {
        let target = SshHop {
            host: ssh_host,
            port: int(row, "sshPort")
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(22),
            user: text(row, "sshUsername").unwrap_or_else(|| "root".into()),
            auth: ssh_auth(text(row, "sshMode").as_deref(), text(row, "sshKeyfile")),
        };
        let jump = text(row, "sshBastionHost").map(|h| SshHop {
            host: h,
            port: int(row, "sshBastionHostPort")
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(22),
            user: text(row, "sshBastionUsername").unwrap_or_else(|| "root".into()),
            auth: ssh_auth(
                text(row, "sshBastionMode").as_deref(),
                text(row, "sshBastionKeyfile"),
            ),
        });
        if let Some(pw) = secret(row, "sshPassword", key) {
            secrets.push((SecretSlot::SshPassword, pw));
        }
        if let Some(pw) = secret(row, "sshKeyfilePassword", key) {
            secrets.push((SecretSlot::SshKeyPassphrase, pw));
        }
        if let Some(pw) = secret(row, "sshBastionPassword", key) {
            secrets.push((SecretSlot::JumpPassword, pw));
        }
        config.ssh = Some(SshConfig { target, jump });
    }
    if let Some(color) = text(row, "labelColor") {
        let known = [
            ("red", "#c0392b"),
            ("orange", "#d35400"),
            ("yellow", "#b7950b"),
            ("green", "#1e5640"),
            ("blue", "#2c5f8a"),
            ("purple", "#6c3483"),
            ("pink", "#a93266"),
        ];
        if let Some((_, hex)) = known.iter().find(|(n, _)| *n == color) {
            config.color = Some((*hex).to_owned());
        }
    }
    report.imported.push(Imported {
        config,
        secrets,
        notes,
    });
}
