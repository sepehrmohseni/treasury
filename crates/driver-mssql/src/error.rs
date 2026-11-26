//! tiberius errors as Treasury errors, by SQL Server error number.

use treasury_core::error::{NetworkCause, ServerMessage};
use treasury_core::{Error, ErrorKind};

pub(crate) fn map(e: &tiberius::error::Error, host: &str, port: u16, sql: Option<&str>) -> Error {
    use tiberius::error::Error as E;
    match e {
        E::Server(t) => {
            let kind = kind_for(t.code(), host, port);
            let position = (kind == ErrorKind::Syntax)
                .then(|| sql.and_then(|q| syntax_position(t.message(), t.line(), q)))
                .flatten();
            let server = ServerMessage {
                code: Some(t.code().to_string()),
                message: t.message().to_owned(),
                position,
            };
            let text = match &kind {
                ErrorKind::Auth => "The server rejected the user name or password.".to_owned(),
                ErrorKind::Syntax => "The statement has a syntax error.".to_owned(),
                ErrorKind::Permission { .. } => {
                    "The user lacks a permission this statement needs.".to_owned()
                }
                _ => t.message().to_owned(),
            };
            Error::new(kind, text).with_server(server)
        }
        E::Io { kind, message } => {
            Error::network(host, port, &std::io::Error::new(*kind, message.clone()))
        }
        E::Tls(m) => Error::new(
            ErrorKind::Tls,
            format!("The TLS connection to {host}:{port} failed: {m}"),
        ),
        E::Routing { host: h, port: p } => Error::new(
            ErrorKind::Network {
                host: host.to_owned(),
                port,
                cause: NetworkCause::Refused,
            },
            format!(
                "The server redirected the connection to {h}:{p}, which cannot be followed through an SSH tunnel."
            ),
        ),
        other => Error::protocol(other.to_string()),
    }
}

fn kind_for(code: u32, host: &str, port: u16) -> ErrorKind {
    match code {
        18456 | 18452 | 18486 | 18487 | 18488 => ErrorKind::Auth,
        102 | 105 | 156 | 170 | 319 => ErrorKind::Syntax,
        229 | 230 | 262 | 297 | 300 | 916 | 1088 | 15247 => ErrorKind::Permission { grant: None },
        3906 => ErrorKind::ReadOnly,
        3980 | 3617 => ErrorKind::Cancelled,
        4060 => ErrorKind::Config {
            field: Some("database".into()),
        },
        233 | 10054 | 6005 => ErrorKind::Network {
            host: host.to_owned(),
            port,
            cause: NetworkCause::Reset,
        },
        _ => ErrorKind::Server,
    }
}

/// Where a syntax error is, from "Incorrect syntax near 'FORM'." and the error's line number:
/// the first occurrence of the quoted text on that line.
fn syntax_position(message: &str, line: u32, sql: &str) -> Option<u32> {
    let start = message.find("near '")? + "near '".len();
    let near = message.get(start..)?.rsplit_once('\'')?.0;
    let line_index = usize::try_from(line.max(1) - 1).ok()?;
    let mut offset = 0usize;
    for (i, l) in sql.split_inclusive('\n').enumerate() {
        if i == line_index {
            let col = l.find(near)?;
            return u32::try_from(sql.get(..offset + col)?.chars().count()).ok();
        }
        offset += l.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_map_to_kinds() {
        assert_eq!(kind_for(18456, "h", 1), ErrorKind::Auth);
        assert_eq!(kind_for(102, "h", 1), ErrorKind::Syntax);
        assert!(matches!(
            kind_for(229, "h", 1),
            ErrorKind::Permission { .. }
        ));
        assert_eq!(kind_for(208, "h", 1), ErrorKind::Server);
    }

    #[test]
    fn syntax_errors_point_at_the_line() {
        assert_eq!(
            syntax_position("Incorrect syntax near 'FORM'.", 1, "SELECT * FORM t"),
            Some(9)
        );
        assert_eq!(
            syntax_position(
                "Incorrect syntax near 'FORM'.",
                2,
                "SELECT 1\nSELECT * FORM t"
            ),
            Some(18)
        );
        assert_eq!(syntax_position("Something else", 1, "x"), None);
    }
}
