//! Redis errors as Treasury errors, keeping the server's own message.

use treasury_core::error::ServerMessage;
use treasury_core::{Error, ErrorKind};

/// `e`, from the server at `host`:`port`.
pub fn map(e: &redis::RedisError, host: &str, port: u16) -> Error {
    use std::io::ErrorKind as Io;
    if e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() {
        let kind = if e.is_connection_refusal() {
            Io::ConnectionRefused
        } else if e.is_timeout() {
            Io::TimedOut
        } else {
            Io::ConnectionReset
        };
        return Error::network(host, port, &std::io::Error::new(kind, e.to_string()));
    }
    let code = e.code().unwrap_or("").to_owned();
    let detail = e.detail().map_or_else(|| e.to_string(), str::to_owned);
    let server = ServerMessage {
        code: (!code.is_empty()).then(|| code.clone()),
        message: detail.clone(),
        position: None,
    };
    let lower = detail.to_ascii_lowercase();
    let kind = match code.as_str() {
        "WRONGPASS" | "NOAUTH" => ErrorKind::Auth,
        "NOPERM" => ErrorKind::Permission { grant: None },
        "READONLY" => ErrorKind::ReadOnly,
        "UNBLOCKED" => return Error::cancelled(),
        "ERR"
            if lower.contains("unknown command")
                || lower.contains("wrong number of arguments")
                || lower.contains("syntax error")
                || lower.contains("unknown subcommand") =>
        {
            ErrorKind::Syntax
        }
        _ if matches!(e.kind(), redis::ErrorKind::AuthenticationFailed) => ErrorKind::Auth,
        _ => ErrorKind::Server,
    };
    let message = match kind {
        ErrorKind::Auth => format!(
            "{host}:{port} refused the user name or password. Check them, and that the user is \
             allowed to sign in ({detail})."
        ),
        ErrorKind::Permission { .. } => format!(
            "The user may not run this command ({detail}). Its ACL rules on the server decide \
             what it may run."
        ),
        _ => detail,
    };
    Error::new(kind, message).with_server(server)
}
