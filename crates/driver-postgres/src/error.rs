//! tokio-postgres errors as Treasury errors, by SQLSTATE.

use tokio_postgres::error::{ErrorPosition, SqlState};
use treasury_core::error::ServerMessage;
use treasury_core::{Error, ErrorKind};

/// Maps a tokio-postgres error. `host` and `port` name the server for network errors.
pub fn map(e: &tokio_postgres::Error, host: &str, port: u16) -> Error {
    if let Some(db) = e.as_db_error() {
        let code = db.code();
        let message = db.message().to_owned();
        let position = match db.position() {
            // PostgreSQL counts characters from 1.
            Some(ErrorPosition::Original(p)) => Some(p.saturating_sub(1)),
            _ => None,
        };
        let server = ServerMessage {
            code: Some(code.code().to_owned()),
            message: match (db.detail(), db.hint()) {
                (Some(d), Some(h)) => format!("{message} ({d} {h})"),
                (Some(d), None) => format!("{message} ({d})"),
                (None, Some(h)) => format!("{message} ({h})"),
                (None, None) => message.clone(),
            },
            position,
        };
        let kind = kind_for(code, &message);
        let text = match &kind {
            ErrorKind::Auth => "The server rejected the user name or password.".to_owned(),
            ErrorKind::Syntax => "The statement has a syntax error.".to_owned(),
            ErrorKind::Permission { .. } => "The user lacks a privilege this statement needs.".to_owned(),
            ErrorKind::ReadOnly => {
                "The connection is read-only, so the server refused to write. Unlock writes to run it.".to_owned()
            }
            ErrorKind::Cancelled => "Cancelled.".to_owned(),
            ErrorKind::Timeout { .. } => "The statement ran past its time limit and was stopped.".to_owned(),
            _ => message,
        };
        return Error::new(kind, text).with_server(server);
    }
    let source = std::error::Error::source(e).and_then(|s| s.downcast_ref::<std::io::Error>());
    if let Some(io) = source {
        if let Some(tls) = io.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
            return tls_error(tls, host, port);
        }
        return Error::network(host, port, io);
    }
    let text = e.to_string();
    if text.contains("timed out") || text.contains("timeout") {
        return Error::network(
            host,
            port,
            &std::io::Error::from(std::io::ErrorKind::TimedOut),
        );
    }
    if e.is_closed() || text.contains("connection closed") {
        return Error::network(
            host,
            port,
            &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
        );
    }
    if text.contains("tls") || text.contains("certificate") || text.contains("TLS") {
        return Error::new(
            ErrorKind::Tls,
            format!("The TLS connection to {host}:{port} failed: {text}."),
        );
    }
    Error::protocol(text)
}

/// A TLS failure, worded for what the user can change.
pub(crate) fn tls_error(e: &rustls::Error, host: &str, port: u16) -> Error {
    let text = match e {
        rustls::Error::InvalidCertificate(why) => format!(
            "The certificate of {host}:{port} is not trusted ({why:?}). Add the CA that issued \
             it in the TLS settings, check the server name, or turn on \"Accept invalid \
             certificates\" for this connection."
        ),
        other => format!("The TLS handshake with {host}:{port} failed: {other}."),
    };
    Error::new(ErrorKind::Tls, text)
}

fn kind_for(code: &SqlState, message: &str) -> ErrorKind {
    let c = code.code();
    if *code == SqlState::INVALID_PASSWORD || c.starts_with("28") {
        ErrorKind::Auth
    } else if *code == SqlState::SYNTAX_ERROR {
        ErrorKind::Syntax
    } else if *code == SqlState::INSUFFICIENT_PRIVILEGE {
        ErrorKind::Permission { grant: None }
    } else if *code == SqlState::READ_ONLY_SQL_TRANSACTION {
        ErrorKind::ReadOnly
    } else if *code == SqlState::QUERY_CANCELED {
        if message.contains("statement timeout") {
            ErrorKind::Timeout { after_ms: 0 }
        } else {
            ErrorKind::Cancelled
        }
    } else if c.starts_with("08")
        || *code == SqlState::ADMIN_SHUTDOWN
        || *code == SqlState::CRASH_SHUTDOWN
    {
        ErrorKind::Network {
            host: String::new(),
            port: 0,
            cause: treasury_core::error::NetworkCause::Reset,
        }
    } else if *code == SqlState::INVALID_CATALOG_NAME {
        ErrorKind::Config {
            field: Some("database".into()),
        }
    } else {
        ErrorKind::Server
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstates() {
        assert_eq!(kind_for(&SqlState::INVALID_PASSWORD, ""), ErrorKind::Auth);
        assert_eq!(kind_for(&SqlState::SYNTAX_ERROR, ""), ErrorKind::Syntax);
        assert_eq!(
            kind_for(&SqlState::READ_ONLY_SQL_TRANSACTION, ""),
            ErrorKind::ReadOnly
        );
        assert_eq!(
            kind_for(
                &SqlState::QUERY_CANCELED,
                "canceling statement due to user request"
            ),
            ErrorKind::Cancelled
        );
        assert!(matches!(
            kind_for(
                &SqlState::QUERY_CANCELED,
                "canceling statement due to statement timeout"
            ),
            ErrorKind::Timeout { .. }
        ));
        assert!(matches!(
            kind_for(&SqlState::ADMIN_SHUTDOWN, ""),
            ErrorKind::Network { .. }
        ));
        assert_eq!(kind_for(&SqlState::UNDEFINED_TABLE, ""), ErrorKind::Server);
    }
}
