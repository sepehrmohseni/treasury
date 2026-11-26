//! mysql_async errors as Treasury errors, by server error code.

use treasury_core::error::{NetworkCause, ServerMessage};
use treasury_core::{Error, ErrorKind};

/// Maps an error. `sql` is the statement, used to point at a syntax error.
pub(crate) fn map(e: &mysql_async::Error, host: &str, port: u16, sql: Option<&str>) -> Error {
    use mysql_async::{DriverError, Error as E, IoError};
    match e {
        E::Server(s) => {
            let kind = kind_for(s.code, host, port);
            let position = (kind == ErrorKind::Syntax)
                .then(|| sql.and_then(|q| syntax_position(&s.message, q)))
                .flatten();
            let server = ServerMessage {
                code: Some(s.code.to_string()),
                message: s.message.clone(),
                position,
            };
            let text = match &kind {
                ErrorKind::Auth => "The server rejected the user name or password.".to_owned(),
                ErrorKind::Syntax => "The statement has a syntax error.".to_owned(),
                ErrorKind::Permission { .. } => {
                    "The user lacks a privilege this statement needs.".to_owned()
                }
                ErrorKind::ReadOnly => "The connection is read-only, so the server refused to \
                                        write. Unlock writes to run it."
                    .to_owned(),
                ErrorKind::Cancelled => "Cancelled.".to_owned(),
                _ => s.message.clone(),
            };
            Error::new(kind, text).with_server(server)
        }
        E::Io(IoError::Io(io)) => Error::network(host, port, io),
        E::Io(IoError::Tls(t)) => Error::new(
            ErrorKind::Tls,
            format!("The TLS connection to {host}:{port} failed: {t}."),
        ),
        E::Driver(DriverError::NoClientSslFlagFromServer) => Error::new(
            ErrorKind::Tls,
            format!(
                "{host}:{port} does not offer TLS. Turn TLS off for this connection or enable it on the server."
            ),
        ),
        E::Driver(DriverError::ConnectionClosed) => Error::network(
            host,
            port,
            &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
        ),
        other => Error::protocol(other.to_string()),
    }
}

fn kind_for(code: u16, host: &str, port: u16) -> ErrorKind {
    match code {
        1045 | 1698 | 1251 | 2059 => ErrorKind::Auth,
        1064 | 1149 => ErrorKind::Syntax,
        1044 | 1142 | 1143 | 1227 | 1370 | 1410 => ErrorKind::Permission { grant: None },
        1290 | 1792 | 1836 => ErrorKind::ReadOnly,
        1317 => ErrorKind::Cancelled,
        3024 | 1969 => ErrorKind::Timeout { after_ms: 0 },
        1049 => ErrorKind::Config {
            field: Some("database".into()),
        },
        1053 | 1927 | 2006 | 2013 | 4031 => ErrorKind::Network {
            host: host.to_owned(),
            port,
            cause: NetworkCause::Reset,
        },
        _ => ErrorKind::Server,
    }
}

/// Where a syntax error is, from MySQL's "... near 'rest of the statement' at line 1": the
/// quoted text is the statement from the error onwards, so its position is where it starts.
fn syntax_position(message: &str, sql: &str) -> Option<u32> {
    let start = message.find("near '")? + "near '".len();
    let end = message.rfind("' at line")?;
    let near = message.get(start..end)?;
    let byte = if near.is_empty() {
        sql.trim_end().len()
    } else {
        sql.find(near)?
    };
    u32::try_from(sql.get(..byte)?.chars().count()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_map_to_kinds() {
        assert_eq!(kind_for(1045, "h", 1), ErrorKind::Auth);
        assert_eq!(kind_for(1064, "h", 1), ErrorKind::Syntax);
        assert_eq!(kind_for(1792, "h", 1), ErrorKind::ReadOnly);
        assert_eq!(kind_for(1317, "h", 1), ErrorKind::Cancelled);
        assert!(matches!(
            kind_for(1142, "h", 1),
            ErrorKind::Permission { .. }
        ));
        assert!(matches!(kind_for(2013, "h", 1), ErrorKind::Network { .. }));
        assert_eq!(kind_for(1146, "h", 1), ErrorKind::Server);
    }

    #[test]
    fn syntax_errors_point_at_the_text() {
        let msg = "You have an error in your SQL syntax; check the manual that corresponds to \
                   your MySQL server version for the right syntax to use near 'FORM t' at line 1";
        assert_eq!(syntax_position(msg, "SELECT * FORM t"), Some(9));
        let at_end = "... for the right syntax to use near '' at line 1";
        assert_eq!(syntax_position(at_end, "SELECT 1 +"), Some(10));
        assert_eq!(syntax_position("no position here", "x"), None);
        // Positions count characters, not bytes.
        assert_eq!(
            syntax_position("near 'FORM t' at line 1", "SELECT 'é' FORM t"),
            Some(11)
        );
    }
}
