//! Parsing pasted connection strings into connection settings.
//!
//! Accepted forms: `scheme://user:password@host:port/database?options` for every engine (with
//! several hosts where the engine has them), the JDBC variants (`jdbc:postgresql://`,
//! `jdbc:sqlserver://host;databaseName=x`, `jdbc:oracle:thin:@//host:1521/service`, ...),
//! SQL Server ADO.NET strings (`Server=tcp:host,1433;Database=x;...`) and PostgreSQL key/value
//! strings (`host=x port=5432 dbname=y`). The password, if present, is returned separately as a
//! secret and never stored in the parsed settings.

use crate::config::{Endpoint, Engine, TlsMode};
use crate::error::{Error, Result};
use secrecy::SecretString;
use std::collections::BTreeMap;

/// What a connection string specified. Fields it did not mention are `None`.
#[derive(Debug, Default)]
pub struct ParsedUri {
    /// The engine the scheme names.
    pub engine: Option<Engine>,
    /// Where the server is.
    pub endpoint: Option<Endpoint>,
    /// Database, keyspace, service name or Redis database number.
    pub database: Option<String>,
    /// User name.
    pub user: Option<String>,
    /// Password or token; kept out of every other field.
    pub password: Option<SecretString>,
    /// TLS mode, when the string says.
    pub tls: Option<TlsMode>,
    /// The string disables certificate verification.
    pub accept_invalid_certs: bool,
    /// Engine-specific settings (`warehouse`, `role`, `authSource`, ...).
    pub options: BTreeMap<String, String>,
    /// Parameters that were not understood and were left out, for the form to mention.
    pub ignored: Vec<String>,
}

/// Parses a pasted connection string.
pub fn parse(input: &str) -> Result<ParsedUri> {
    let s = input.trim();
    if s.is_empty() {
        return Err(bad("The connection string is empty."));
    }
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = strip_prefix_ci(s, "jdbc:") {
        return parse_jdbc(rest);
    }
    if lower.contains("://") {
        return parse_url(s);
    }
    if lower.starts_with("server=")
        || lower.starts_with("data source=")
        || lower.contains(";server=")
        || lower.contains(";data source=")
    {
        return parse_ado(s);
    }
    if lower.starts_with("host=") || lower.contains(" host=") || lower.starts_with("dbname=") {
        return parse_pg_keyvalue(s);
    }
    Err(bad(
        "Treasury does not recognize this connection string. Paste a URI such as \
         postgres://user@host:5432/db, or fill in the form.",
    ))
}

fn bad(message: impl Into<String>) -> Error {
    Error::config(Some("uri"), message)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` returns None instead of panicking when the prefix length splits a character.
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        s.get(prefix.len()..)
    } else {
        None
    }
}

fn engine_for_scheme(scheme: &str) -> Option<(Engine, Option<TlsMode>)> {
    Some(match scheme {
        "postgres" | "postgresql" => (Engine::Postgres, None),
        "mysql" => (Engine::MySql, None),
        "mariadb" => (Engine::MariaDb, None),
        "sqlserver" | "mssql" => (Engine::SqlServer, None),
        "mongodb" | "mongodb+srv" => (Engine::MongoDb, None),
        "redis" => (Engine::Redis, Some(TlsMode::Disable)),
        "rediss" => (Engine::Redis, Some(TlsMode::Require)),
        "valkey" => (Engine::Valkey, Some(TlsMode::Disable)),
        "valkeys" => (Engine::Valkey, Some(TlsMode::Require)),
        "cassandra" => (Engine::Cassandra, None),
        "scylla" | "scylladb" => (Engine::ScyllaDb, None),
        "snowflake" => (Engine::Snowflake, Some(TlsMode::Require)),
        "databricks" => (Engine::Databricks, Some(TlsMode::Require)),
        "oracle" => (Engine::Oracle, None),
        "db2" => (Engine::Db2, None),
        _ => return None,
    })
}

/// The pieces of `scheme://userinfo@hosts/path?query#fragment`.
struct UrlParts<'a> {
    scheme: String,
    user: Option<String>,
    password: Option<String>,
    hosts: Vec<(String, Option<u16>)>,
    path: &'a str,
    query: Vec<(String, String)>,
}

fn split_url(s: &str) -> Result<UrlParts<'_>> {
    let (scheme, rest) = s
        .split_once("://")
        .ok_or_else(|| bad("The connection string has no scheme."))?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let (before_query, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let (authority, path) = match before_query.find('/') {
        Some(i) => (&before_query[..i], &before_query[i..]),
        None => (before_query, ""),
    };
    let (userinfo, hostlist) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (Some(decode(u)?), Some(decode(p)?)),
            None => (Some(decode(ui)?), None),
        },
        None => (None, None),
    };
    let mut hosts = Vec::new();
    for part in hostlist.split(',').filter(|p| !p.is_empty()) {
        hosts.push(split_host_port(part)?);
    }
    let mut pairs = Vec::new();
    for kv in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        pairs.push((decode(k)?, decode(v)?));
    }
    Ok(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        user: user.filter(|u| !u.is_empty()),
        password,
        hosts,
        path,
        query: pairs,
    })
}

fn split_host_port(part: &str) -> Result<(String, Option<u16>)> {
    if let Some(rest) = part.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| bad("An IPv6 address is missing its closing bracket."))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None => None,
        };
        return Ok((host.to_owned(), port));
    }
    match part.rsplit_once(':') {
        Some((h, p)) => Ok((decode(h)?, Some(parse_port(p)?))),
        None => Ok((decode(part)?, None)),
    }
}

fn parse_port(p: &str) -> Result<u16> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(Error::config(
            Some("port"),
            format!("\"{p}\" is not a port number between 1 and 65535."),
        )),
    }
}

/// Percent-decodes `s` (and `+` stays `+`, as in URI userinfo and paths).
fn decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| bad("The connection string has an invalid percent escape."))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad("The connection string is not valid UTF-8."))
}

/// Percent-encodes the characters that are special in URI userinfo.
pub fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn parse_url(s: &str) -> Result<ParsedUri> {
    let parts = split_url(s)?;
    let (engine, scheme_tls) = engine_for_scheme(&parts.scheme).ok_or_else(|| {
        bad(format!(
            "Treasury does not know the scheme \"{}\".",
            parts.scheme
        ))
    })?;
    let mut out = ParsedUri {
        engine: Some(engine),
        user: parts.user.clone(),
        password: parts.password.clone().map(SecretString::from),
        tls: scheme_tls,
        ..ParsedUri::default()
    };
    let path = parts.path.trim_start_matches('/');
    match engine {
        Engine::MongoDb => {
            out.endpoint = Some(Endpoint::Uri {
                uri: without_password(s, &parts),
            });
            if !path.is_empty() {
                out.database = Some(decode(path)?);
            }
            for (k, v) in &parts.query {
                match k.to_ascii_lowercase().as_str() {
                    "tls" | "ssl" => out.tls = Some(bool_tls(v)),
                    "tlsallowinvalidcertificates" | "tlsinsecure" => {
                        out.accept_invalid_certs = is_true(v)
                    }
                    _ => {}
                }
            }
            return Ok(out);
        }
        Engine::Snowflake => {
            let host = parts.hosts.first().map(|(h, _)| h.as_str()).unwrap_or("");
            let account = host
                .strip_suffix(".snowflakecomputing.com")
                .unwrap_or(host)
                .to_owned();
            out.endpoint = Some(Endpoint::Account { account });
            let mut segs = path.split('/').filter(|p| !p.is_empty());
            if let Some(db) = segs.next() {
                out.database = Some(decode(db)?);
            }
            if let Some(schema) = segs.next() {
                out.options.insert("schema".into(), decode(schema)?);
            }
        }
        Engine::Databricks => {
            let host = parts
                .hosts
                .first()
                .map(|(h, _)| h.clone())
                .unwrap_or_default();
            let http_path = parts
                .query
                .iter()
                .find(|(k, _)| {
                    k.eq_ignore_ascii_case("http_path") || k.eq_ignore_ascii_case("httpPath")
                })
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| format!("/{path}"));
            out.endpoint = Some(Endpoint::Workspace { host, http_path });
            if out.user.as_deref() == Some("token") {
                out.user = None;
            }
        }
        Engine::Redis | Engine::Valkey => {
            out.endpoint = Some(hosts_endpoint(&parts.hosts, engine));
            if !path.is_empty() {
                out.database = Some(decode(path)?);
            }
        }
        _ => {
            out.endpoint = Some(hosts_endpoint(&parts.hosts, engine));
            if !path.is_empty() {
                out.database = Some(decode(path)?);
            }
        }
    }
    for (k, v) in &parts.query {
        apply_option(engine, k, v, &mut out);
    }
    Ok(out)
}

fn hosts_endpoint(hosts: &[(String, Option<u16>)], engine: Engine) -> Endpoint {
    let port = |p: &Option<u16>| p.unwrap_or(engine.default_port());
    match hosts {
        [] => Endpoint::Host {
            host: "localhost".into(),
            port: engine.default_port(),
        },
        [(h, p)] => Endpoint::Host {
            host: h.clone(),
            port: port(p),
        },
        many => Endpoint::Hosts {
            hosts: many.iter().map(|(h, p)| (h.clone(), port(p))).collect(),
        },
    }
}

/// The original string with the password removed, for engines whose driver parses URIs.
fn without_password(s: &str, parts: &UrlParts<'_>) -> String {
    if parts.password.is_none() {
        return s.to_owned();
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return s.to_owned();
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            let user = userinfo.split(':').next().unwrap_or("");
            format!("{scheme}://{user}@{}{tail}", &authority[at + 1..])
        }
        None => s.to_owned(),
    }
}

fn is_true(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
}

fn bool_tls(v: &str) -> TlsMode {
    if is_true(v) {
        TlsMode::Require
    } else {
        TlsMode::Disable
    }
}

fn apply_option(engine: Engine, key: &str, value: &str, out: &mut ParsedUri) {
    let k = key.to_ascii_lowercase();
    match (engine, k.as_str()) {
        (Engine::Postgres, "sslmode") => match value {
            "disable" => out.tls = Some(TlsMode::Disable),
            "allow" | "prefer" => out.tls = Some(TlsMode::Prefer),
            "require" => {
                // libpq's `require` encrypts without checking the certificate.
                out.tls = Some(TlsMode::Require);
                out.accept_invalid_certs = true;
            }
            "verify-ca" | "verify-full" => out.tls = Some(TlsMode::Require),
            other => out.ignored.push(format!("sslmode={other}")),
        },
        (Engine::Postgres, "application_name" | "connect_timeout" | "options") => {
            out.options.insert(k, value.to_owned());
        }
        (Engine::MySql | Engine::MariaDb, "ssl-mode" | "sslmode") => {
            match value.to_ascii_uppercase().as_str() {
                "DISABLED" => out.tls = Some(TlsMode::Disable),
                "PREFERRED" => out.tls = Some(TlsMode::Prefer),
                "REQUIRED" => {
                    out.tls = Some(TlsMode::Require);
                    out.accept_invalid_certs = true;
                }
                "VERIFY_CA" | "VERIFY_IDENTITY" => out.tls = Some(TlsMode::Require),
                other => out.ignored.push(format!("{key}={other}")),
            }
        }
        (Engine::MySql | Engine::MariaDb, "usessl") => out.tls = Some(bool_tls(value)),
        (Engine::SqlServer, "encrypt") => out.tls = Some(bool_tls(value)),
        (Engine::SqlServer, "trustservercertificate") => out.accept_invalid_certs = is_true(value),
        (Engine::SqlServer, "database" | "databasename") => out.database = Some(value.to_owned()),
        (Engine::SqlServer, "user" | "username" | "userid") => out.user = Some(value.to_owned()),
        (Engine::SqlServer, "password") => {
            out.password = Some(SecretString::from(value.to_owned()))
        }
        (Engine::Snowflake, "warehouse" | "role" | "schema" | "authenticator") => {
            out.options.insert(k, value.to_owned());
        }
        (Engine::Snowflake, "db" | "database") => out.database = Some(value.to_owned()),
        (Engine::Databricks, "http_path" | "httppath") => {}
        (Engine::Databricks, "catalog" | "schema") => {
            out.options.insert(k, value.to_owned());
        }
        (Engine::Redis | Engine::Valkey, "mode" | "master_name") => {
            out.options.insert(k, value.to_owned());
        }
        (Engine::Cassandra | Engine::ScyllaDb, "dc" | "local_dc" | "consistency") => {
            out.options.insert(k, value.to_owned());
        }
        (_, "tls" | "ssl") => out.tls = Some(bool_tls(value)),
        _ => out.ignored.push(key.to_owned()),
    }
}

fn parse_jdbc(rest: &str) -> Result<ParsedUri> {
    let lower = rest.to_ascii_lowercase();
    if let Some(after) = strip_prefix_ci(rest, "oracle:thin:@") {
        // `//host:port/service` or `host:port/service` or `host:port:SID`.
        let addr = after.trim_start_matches("//");
        let (hostport, service) = match addr.split_once('/') {
            Some((hp, svc)) => (hp, Some(svc.to_owned())),
            None => (addr, None),
        };
        let mut out = ParsedUri {
            engine: Some(Engine::Oracle),
            ..ParsedUri::default()
        };
        let pieces: Vec<&str> = hostport.split(':').collect();
        let (host, port, sid) = match pieces.as_slice() {
            [h] => (*h, None, None),
            [h, p] => (*h, Some(parse_port(p)?), None),
            [h, p, sid] => (*h, Some(parse_port(p)?), Some((*sid).to_owned())),
            _ => return Err(bad("The Oracle JDBC address is not host:port/service.")),
        };
        out.endpoint = Some(Endpoint::Host {
            host: host.to_owned(),
            port: port.unwrap_or(Engine::Oracle.default_port()),
        });
        out.database = service.or(sid);
        return Ok(out);
    }
    if lower.starts_with("sqlserver://") {
        // jdbc:sqlserver://host[\instance][:port];key=value;...
        let body = &rest["sqlserver://".len()..];
        let mut segs = body.split(';');
        let addr = segs.next().unwrap_or("");
        let mut out = ParsedUri {
            engine: Some(Engine::SqlServer),
            ..ParsedUri::default()
        };
        let (host, port) = split_host_port(addr)?;
        out.endpoint = Some(Endpoint::Host {
            host,
            port: port.unwrap_or(Engine::SqlServer.default_port()),
        });
        for seg in segs.filter(|s| !s.is_empty()) {
            let (k, v) = seg.split_once('=').unwrap_or((seg, ""));
            apply_option(Engine::SqlServer, k.trim(), v.trim(), &mut out);
        }
        return Ok(out);
    }
    if lower.starts_with("databricks://") {
        // jdbc:databricks://host:443/default;httpPath=/sql/1.0/...;AuthMech=3;UID=token;PWD=...
        let body = &rest["databricks://".len()..];
        let mut segs = body.split(';');
        let addr = segs.next().unwrap_or("");
        let host = addr.split(['/', ':']).next().unwrap_or("").to_owned();
        let mut http_path = String::new();
        let mut out = ParsedUri {
            engine: Some(Engine::Databricks),
            tls: Some(TlsMode::Require),
            ..ParsedUri::default()
        };
        for seg in segs.filter(|s| !s.is_empty()) {
            let (k, v) = seg.split_once('=').unwrap_or((seg, ""));
            match k.trim().to_ascii_lowercase().as_str() {
                "httppath" => http_path = v.trim().to_owned(),
                "pwd" => out.password = Some(SecretString::from(v.trim().to_owned())),
                "uid" | "authmech" | "transportmode" | "ssl" => {}
                "conncatalog" => {
                    out.options.insert("catalog".into(), v.trim().to_owned());
                }
                "connschema" => {
                    out.options.insert("schema".into(), v.trim().to_owned());
                }
                _ => out.ignored.push(k.trim().to_owned()),
            }
        }
        out.endpoint = Some(Endpoint::Workspace { host, http_path });
        return Ok(out);
    }
    if lower.starts_with("snowflake://") {
        return parse_url(rest);
    }
    // jdbc:postgresql://, jdbc:mysql://, jdbc:mariadb://, jdbc:db2://
    let mut out = parse_url(rest)?;
    if out.engine == Some(Engine::Db2) {
        // Db2 JDBC puts properties after a colon: jdbc:db2://h:50000/DB:user=x;password=y;
        if let Some(db) = out.database.take() {
            let (name, props) = db.split_once(':').unwrap_or((db.as_str(), ""));
            out.database = Some(name.to_owned());
            for seg in props.split(';').filter(|s| !s.is_empty()) {
                let (k, v) = seg.split_once('=').unwrap_or((seg, ""));
                match k.to_ascii_lowercase().as_str() {
                    "user" => out.user = Some(v.to_owned()),
                    "password" => out.password = Some(SecretString::from(v.to_owned())),
                    "sslconnection" => out.tls = Some(bool_tls(v)),
                    _ => out.ignored.push(k.to_owned()),
                }
            }
        }
    }
    Ok(out)
}

fn parse_ado(s: &str) -> Result<ParsedUri> {
    let mut out = ParsedUri {
        engine: Some(Engine::SqlServer),
        ..ParsedUri::default()
    };
    for seg in s.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((k, v)) = seg.split_once('=') else {
            out.ignored.push(seg.to_owned());
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k.trim().to_ascii_lowercase().as_str() {
            "server" | "data source" | "address" | "addr" => {
                let addr = v.strip_prefix("tcp:").unwrap_or(v);
                let (host, port) = match addr.rsplit_once(',') {
                    Some((h, p)) => (h.to_owned(), parse_port(p.trim())?),
                    None => (addr.to_owned(), Engine::SqlServer.default_port()),
                };
                out.endpoint = Some(Endpoint::Host { host, port });
            }
            "database" | "initial catalog" => out.database = Some(v.to_owned()),
            "user id" | "uid" | "user" => out.user = Some(v.to_owned()),
            "password" | "pwd" => out.password = Some(SecretString::from(v.to_owned())),
            "encrypt" => {
                out.tls = Some(match v.to_ascii_lowercase().as_str() {
                    "false" | "no" | "optional" => TlsMode::Prefer,
                    _ => TlsMode::Require,
                })
            }
            "trustservercertificate" => out.accept_invalid_certs = is_true(v),
            other => out.ignored.push(other.to_owned()),
        }
    }
    if out.endpoint.is_none() {
        return Err(bad("The connection string has no Server."));
    }
    Ok(out)
}

fn parse_pg_keyvalue(s: &str) -> Result<ParsedUri> {
    let mut out = ParsedUri {
        engine: Some(Engine::Postgres),
        ..ParsedUri::default()
    };
    let mut host = None;
    let mut port = None;
    for (k, v) in keyvalue_pairs(s)? {
        match k.as_str() {
            "host" | "hostaddr" => host = Some(v),
            "port" => port = Some(parse_port(&v)?),
            "dbname" => out.database = Some(v),
            "user" => out.user = Some(v),
            "password" => out.password = Some(SecretString::from(v)),
            _ => apply_option(Engine::Postgres, &k, &v, &mut out),
        }
    }
    out.endpoint = Some(Endpoint::Host {
        host: host.unwrap_or_else(|| "localhost".into()),
        port: port.unwrap_or(Engine::Postgres.default_port()),
    });
    Ok(out)
}

/// Splits libpq `key=value key2='quoted value'` pairs.
fn keyvalue_pairs(s: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') {
            return Err(bad(format!("\"{key}\" has no value.")));
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    Some('\\') => {
                        if let Some(c) = chars.next() {
                            value.push(c);
                        }
                    }
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => return Err(bad("A quoted value is missing its closing quote.")),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        pairs.push((key.to_ascii_lowercase(), value));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use secrecy::ExposeSecret;

    fn pw(p: &ParsedUri) -> Option<&str> {
        p.password.as_ref().map(|s| s.expose_secret())
    }

    #[test]
    fn postgres_url() {
        let p = parse("postgres://app:s%40cret@db.internal:6543/orders?sslmode=verify-full&application_name=x").unwrap();
        assert_eq!(p.engine, Some(Engine::Postgres));
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Host {
                host: "db.internal".into(),
                port: 6543
            })
        );
        assert_eq!(p.database.as_deref(), Some("orders"));
        assert_eq!(p.user.as_deref(), Some("app"));
        assert_eq!(pw(&p), Some("s@cret"));
        assert_eq!(p.tls, Some(TlsMode::Require));
        assert!(!p.accept_invalid_certs);
        assert_eq!(
            p.options.get("application_name").map(String::as_str),
            Some("x")
        );
    }

    #[test]
    fn libpq_require_does_not_verify() {
        let p = parse("postgresql://h/db?sslmode=require").unwrap();
        assert_eq!(p.tls, Some(TlsMode::Require));
        assert!(p.accept_invalid_certs);
    }

    #[test]
    fn ipv6_and_default_port() {
        let p = parse("mysql://root@[::1]/shop").unwrap();
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Host {
                host: "::1".into(),
                port: 3306
            })
        );
    }

    #[test]
    fn mongo_keeps_uri_without_password() {
        let p =
            parse("mongodb+srv://ana:pa%3Ass@cluster0.example.net/app?retryWrites=true&tls=true")
                .unwrap();
        assert_eq!(p.engine, Some(Engine::MongoDb));
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Uri {
                uri: "mongodb+srv://ana@cluster0.example.net/app?retryWrites=true&tls=true".into()
            })
        );
        assert_eq!(pw(&p), Some("pa:ss"));
        assert_eq!(p.database.as_deref(), Some("app"));
        assert_eq!(p.tls, Some(TlsMode::Require));
    }

    #[test]
    fn mongo_replica_set_hosts_survive() {
        let p = parse("mongodb://h1:27017,h2:27018/?replicaSet=rs0").unwrap();
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Uri {
                uri: "mongodb://h1:27017,h2:27018/?replicaSet=rs0".into()
            })
        );
    }

    #[test]
    fn redis_schemes_set_tls_and_database() {
        let p = parse("rediss://default:pw@cache:6380/2").unwrap();
        assert_eq!(p.engine, Some(Engine::Redis));
        assert_eq!(p.tls, Some(TlsMode::Require));
        assert_eq!(p.database.as_deref(), Some("2"));
        assert_eq!(p.user.as_deref(), Some("default"));
        let v = parse("valkey://localhost").unwrap();
        assert_eq!(v.engine, Some(Engine::Valkey));
        assert_eq!(v.tls, Some(TlsMode::Disable));
    }

    #[test]
    fn cassandra_seed_hosts() {
        let p = parse("cassandra://u:p@n1,n2:9043/ks?dc=eu").unwrap();
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Hosts {
                hosts: vec![("n1".into(), 9042), ("n2".into(), 9043)]
            })
        );
        assert_eq!(p.database.as_deref(), Some("ks"));
        assert_eq!(p.options.get("dc").map(String::as_str), Some("eu"));
    }

    #[test]
    fn snowflake_account_from_host() {
        let p = parse("snowflake://ana:pw@myorg-acct.snowflakecomputing.com/SALES/PUBLIC?warehouse=WH&role=ANALYST").unwrap();
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Account {
                account: "myorg-acct".into()
            })
        );
        assert_eq!(p.database.as_deref(), Some("SALES"));
        assert_eq!(p.options.get("schema").map(String::as_str), Some("PUBLIC"));
        assert_eq!(p.options.get("warehouse").map(String::as_str), Some("WH"));
    }

    #[test]
    fn databricks_url_and_jdbc() {
        let p = parse("databricks://token:dapi123@adb-1.2.azuredatabricks.net?http_path=/sql/1.0/warehouses/abc").unwrap();
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Workspace {
                host: "adb-1.2.azuredatabricks.net".into(),
                http_path: "/sql/1.0/warehouses/abc".into()
            })
        );
        assert_eq!(p.user, None);
        assert_eq!(pw(&p), Some("dapi123"));
        let j = parse("jdbc:databricks://adb-1.azuredatabricks.net:443/default;transportMode=http;ssl=1;httpPath=/sql/1.0/warehouses/xyz;AuthMech=3;UID=token;PWD=dapiX").unwrap();
        assert_eq!(
            j.endpoint,
            Some(Endpoint::Workspace {
                host: "adb-1.azuredatabricks.net".into(),
                http_path: "/sql/1.0/warehouses/xyz".into()
            })
        );
        assert_eq!(pw(&j), Some("dapiX"));
    }

    #[test]
    fn sql_server_forms() {
        let j = parse("jdbc:sqlserver://sql.internal:1444;databaseName=crm;user=sa;password=x;encrypt=true;trustServerCertificate=true").unwrap();
        assert_eq!(j.engine, Some(Engine::SqlServer));
        assert_eq!(
            j.endpoint,
            Some(Endpoint::Host {
                host: "sql.internal".into(),
                port: 1444
            })
        );
        assert_eq!(j.database.as_deref(), Some("crm"));
        assert_eq!(j.user.as_deref(), Some("sa"));
        assert!(j.accept_invalid_certs);
        let a = parse("Server=tcp:myserver.database.windows.net,1433;Initial Catalog=db;User ID=me;Password=pw;Encrypt=True;").unwrap();
        assert_eq!(pw(&a), Some("pw"));
        assert_eq!(
            a.endpoint,
            Some(Endpoint::Host {
                host: "myserver.database.windows.net".into(),
                port: 1433
            })
        );
        assert_eq!(a.database.as_deref(), Some("db"));
        assert_eq!(a.tls, Some(TlsMode::Require));
    }

    #[test]
    fn oracle_forms() {
        let j = parse("jdbc:oracle:thin:@//ora.internal:1522/ORCLPDB1").unwrap();
        assert_eq!(j.engine, Some(Engine::Oracle));
        assert_eq!(
            j.endpoint,
            Some(Endpoint::Host {
                host: "ora.internal".into(),
                port: 1522
            })
        );
        assert_eq!(j.database.as_deref(), Some("ORCLPDB1"));
        let sid = parse("jdbc:oracle:thin:@ora:1521:XE").unwrap();
        assert_eq!(sid.database.as_deref(), Some("XE"));
        let u = parse("oracle://scott:tiger@ora/FREEPDB1").unwrap();
        assert_eq!(u.database.as_deref(), Some("FREEPDB1"));
    }

    #[test]
    fn db2_jdbc_properties() {
        let p = parse(
            "jdbc:db2://db2.internal:50001/SAMPLE:user=db2inst1;password=pw;sslConnection=true;",
        )
        .unwrap();
        assert_eq!(p.engine, Some(Engine::Db2));
        assert_eq!(p.database.as_deref(), Some("SAMPLE"));
        assert_eq!(p.user.as_deref(), Some("db2inst1"));
        assert_eq!(pw(&p), Some("pw"));
        assert_eq!(p.tls, Some(TlsMode::Require));
    }

    #[test]
    fn libpq_keyvalue() {
        let p =
            parse(r"host=db port=5433 dbname='my db' user=ana password='it\'s' sslmode=disable")
                .unwrap();
        assert_eq!(pw(&p), Some("it's"));
        assert_eq!(
            p.endpoint,
            Some(Endpoint::Host {
                host: "db".into(),
                port: 5433
            })
        );
        assert_eq!(p.database.as_deref(), Some("my db"));
        assert_eq!(p.tls, Some(TlsMode::Disable));
    }

    #[test]
    fn unknown_and_broken_input() {
        assert!(parse("").is_err());
        assert!(parse("ftp://x").is_err());
        assert!(parse("just some words").is_err());
        assert!(parse("postgres://h:99999/db").is_err());
        assert!(parse("postgres://u:%zz@h/db").is_err());
        let p = parse("postgres://h/db?frobnicate=1").unwrap();
        assert_eq!(p.ignored, vec!["frobnicate".to_owned()]);
    }

    #[test]
    fn multibyte_input_near_a_prefix_is_rejected_not_panicked_on() {
        assert!(parse("AA0 \u{a1}").is_err());
        assert!(parse("jdb\u{e9}:x").is_err());
    }

    #[test]
    fn password_never_appears_in_debug() {
        let p = parse("postgres://u:hunter2@h/db").unwrap();
        assert!(!format!("{p:?}").contains("hunter2"));
    }

    proptest! {
        #[test]
        fn url_round_trip(
            user in "[a-zA-Z0-9 @:/?#%._-]{1,16}",
            password in "[a-zA-Z0-9 @:/?#%._-]{0,16}",
            host in "[a-z][a-z0-9-]{0,20}(\\.[a-z]{2,6})?",
            port in 1u16..,
            db in "[a-zA-Z0-9_]{1,12}",
        ) {
            let uri = format!(
                "postgres://{}:{}@{host}:{port}/{db}",
                encode_userinfo(&user),
                encode_userinfo(&password)
            );
            let p = parse(&uri).unwrap();
            prop_assert_eq!(p.user.as_deref(), Some(user.as_str()));
            prop_assert_eq!(pw(&p), Some(password.as_str()));
            prop_assert_eq!(p.endpoint, Some(Endpoint::Host { host, port }));
            prop_assert_eq!(p.database, Some(db));
        }

        #[test]
        fn never_panics(s in "\\PC{0,80}") {
            let _ = parse(&s);
        }
    }
}
