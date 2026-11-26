//! Errors shared by every driver.
//!
//! An [`Error`] carries a [`ErrorKind`] with the structured facts the UI needs to explain it
//! (host and port, the server's own message and error position, the grant that is missing), so
//! the message catalog in the UI can say what happened and what to do next. The English
//! `Display` text is what the CLI and the logs show. Neither ever contains a secret: drivers
//! build errors from server responses and connection metadata, never from credentials.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Result type of every core and driver operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, in a form the UI can act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ErrorKind {
    /// The server rejected the credentials.
    Auth,
    /// The host could not be reached or the connection dropped.
    Network {
        /// The host that was being contacted.
        host: String,
        /// Its port.
        port: u16,
        /// What the network stack reported (refused, reset, unreachable, DNS failure...).
        cause: NetworkCause,
    },
    /// The TLS handshake failed or the certificate was not trusted.
    Tls,
    /// The statement could not be parsed by the server.
    Syntax,
    /// The user lacks a privilege.
    Permission {
        /// The grant that would allow it, when the driver knows (for example `pg_read_all_stats`).
        grant: Option<String>,
    },
    /// The operation ran past its time limit.
    Timeout {
        /// The limit, in milliseconds.
        after_ms: u64,
    },
    /// The user cancelled it.
    Cancelled,
    /// The statement would write on a read-only connection.
    ReadOnly,
    /// A guardrail stopped the statement; see the message for which one.
    Guard,
    /// The engine or this driver does not support the operation.
    Unsupported,
    /// The server sent something the driver could not decode.
    Protocol,
    /// The connection settings are invalid.
    Config {
        /// The setting at fault, when there is one field to point at.
        field: Option<String>,
    },
    /// A client library the engine needs is not installed (Oracle Instant Client, IBM CLI).
    MissingClientLibrary {
        /// The library's name.
        library: String,
        /// Where the vendor offers it.
        download_url: String,
    },
    /// The server reported an error that fits none of the above.
    Server,
    /// A bug in Treasury or a driver, including a panic caught at the task boundary.
    Internal,
}

/// The network failure, as far as the OS told us.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkCause {
    /// Nothing listens on that port, or a firewall rejected the connection.
    Refused,
    /// The host name did not resolve.
    Dns,
    /// No answer within the connect timeout.
    Unreachable,
    /// The connection was open and then closed by the other side.
    Reset,
    /// Something else.
    Other,
}

/// The error as the server reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerMessage {
    /// The engine's error code (SQLSTATE, MySQL error number, MongoDB code name...).
    pub code: Option<String>,
    /// The server's message, verbatim.
    pub message: String,
    /// Character offset of the error in the statement text, counted from 0, when reported.
    pub position: Option<u32>,
}

/// An error from the core or a driver. Its fields are reached through [`ErrorDetail`]; the
/// detail is boxed so that `Result`s stay small.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Error(Box<ErrorDetail>);

/// What an [`Error`] says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// What kind of failure this is.
    pub kind: ErrorKind,
    /// A short English description of what happened, without secrets.
    pub message: String,
    /// The server's own report, when the server produced the error.
    pub server: Option<ServerMessage>,
}

impl std::ops::Deref for Error {
    type Target = ErrorDetail;

    fn deref(&self) -> &ErrorDetail {
        &self.0
    }
}

impl Error {
    /// A new error of `kind` with an English `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(Box::new(ErrorDetail {
            kind,
            message: message.into(),
            server: None,
        }))
    }

    /// Attaches the server's report.
    pub fn with_server(mut self, server: ServerMessage) -> Self {
        self.0.server = Some(server);
        self
    }

    /// An internal error: a bug, never the user's fault.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// The operation is not supported by this engine or driver.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    /// The server sent data the driver could not decode.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    /// An invalid connection setting.
    pub fn config(field: Option<&str>, message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Config {
                field: field.map(str::to_owned),
            },
            message,
        )
    }

    /// The user cancelled the operation.
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Cancelled.")
    }

    /// The operation took longer than `after`.
    pub fn timeout(after: std::time::Duration) -> Self {
        let ms = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        Self::new(
            ErrorKind::Timeout { after_ms: ms },
            format!("Timed out after {}.", human_duration(after)),
        )
    }

    /// A network failure reaching `host:port`, classified from an I/O error.
    pub fn network(host: &str, port: u16, err: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let cause = match err.kind() {
            K::ConnectionRefused => NetworkCause::Refused,
            K::TimedOut => NetworkCause::Unreachable,
            K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
                NetworkCause::Reset
            }
            K::HostUnreachable | K::NetworkUnreachable => NetworkCause::Unreachable,
            _ if is_dns_failure(err) => NetworkCause::Dns,
            _ => NetworkCause::Other,
        };
        let message = match cause {
            NetworkCause::Refused => format!(
                "Connection refused by {host}:{port}. Check the host and port, and that the \
                 server accepts connections from this network."
            ),
            NetworkCause::Dns => {
                format!("The host name {host} could not be resolved. Check the spelling and DNS.")
            }
            NetworkCause::Unreachable => format!(
                "No response from {host}:{port}. Check that the host is up and reachable from \
                 this network."
            ),
            NetworkCause::Reset => {
                format!("The connection to {host}:{port} was closed by the other side.")
            }
            NetworkCause::Other => format!("Could not reach {host}:{port}: {err}."),
        };
        Self::new(
            ErrorKind::Network {
                host: host.to_owned(),
                port,
                cause,
            },
            message,
        )
    }

    /// True when retrying on a fresh connection might succeed.
    pub fn is_connection_lost(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Network {
                cause: NetworkCause::Reset | NetworkCause::Unreachable,
                ..
            }
        )
    }
}

fn is_dns_failure(err: &std::io::Error) -> bool {
    // Resolver failures arrive as `Other`/`Uncategorized` with a platform message.
    let text = err.to_string().to_ascii_lowercase();
    text.contains("failed to lookup address")
        || text.contains("name or service not known")
        || text.contains("nodename nor servname")
        || text.contains("no such host")
        || text.contains("temporary failure in name resolution")
}

/// `250 ms`, `3.5 s`, `2 min 5 s`.
pub fn human_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        let tenths = ms / 100;
        if tenths.is_multiple_of(10) {
            format!("{} s", tenths / 10)
        } else {
            format!("{}.{} s", tenths / 10, tenths % 10)
        }
    } else {
        let secs = ms / 1_000;
        format!("{} min {} s", secs / 60, secs % 60)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(server) = &self.server
            && !self.message.contains(&server.message)
        {
            write!(f, " The server said: {}", server.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn refused_connection_says_what_to_do() {
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = Error::network("db.internal", 5432, &io);
        assert_eq!(
            e.to_string(),
            "Connection refused by db.internal:5432. Check the host and port, and that the \
             server accepts connections from this network."
        );
        assert!(matches!(
            e.kind,
            ErrorKind::Network {
                cause: NetworkCause::Refused,
                port: 5432,
                ..
            }
        ));
    }

    #[test]
    fn server_message_is_appended_once() {
        let e = Error::new(ErrorKind::Syntax, "The statement has a syntax error.").with_server(
            ServerMessage {
                code: Some("42601".into()),
                message: "syntax error at or near \"FORM\"".into(),
                position: Some(9),
            },
        );
        assert_eq!(
            e.to_string(),
            "The statement has a syntax error. The server said: syntax error at or near \"FORM\""
        );
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(Duration::from_millis(250)), "250 ms");
        assert_eq!(human_duration(Duration::from_millis(3_500)), "3.5 s");
        assert_eq!(human_duration(Duration::from_secs(2)), "2 s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2 min 5 s");
        assert_eq!(
            Error::timeout(Duration::from_secs(5)).to_string(),
            "Timed out after 5 s."
        );
    }

    #[test]
    fn lost_connections_are_retryable() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(Error::network("h", 1, &reset).is_connection_lost());
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(!Error::network("h", 1, &refused).is_connection_lost());
    }
}
