//! MongoDB driver errors as Treasury errors.

use mongodb::error::ErrorKind as K;
use treasury_core::error::{NetworkCause, ServerMessage};
use treasury_core::{Error, ErrorKind};

pub(crate) fn map(e: &mongodb::error::Error, host: &str, port: u16) -> Error {
    match e.kind.as_ref() {
        K::Authentication { message, .. } => Error::new(
            ErrorKind::Auth,
            format!("The server rejected the credentials: {message}"),
        ),
        K::Command(c) => {
            let kind = match c.code {
                18 => ErrorKind::Auth,
                13 => ErrorKind::Permission { grant: None },
                11601 => ErrorKind::Cancelled,
                50 => ErrorKind::Timeout { after_ms: 0 },
                9 | 2 if c.message.contains("parse") => ErrorKind::Syntax,
                59 => ErrorKind::Unsupported,
                _ => ErrorKind::Server,
            };
            let text = match &kind {
                ErrorKind::Permission { .. } => {
                    "The user lacks a role this command needs.".to_owned()
                }
                ErrorKind::Cancelled => "Cancelled.".to_owned(),
                _ => c.message.clone(),
            };
            Error::new(kind, text).with_server(ServerMessage {
                code: Some(format!("{} {}", c.code, c.code_name)),
                message: c.message.clone(),
                position: None,
            })
        }
        K::Write(w) => {
            let (code, message) = match w {
                mongodb::error::WriteFailure::WriteError(we) => (we.code, we.message.clone()),
                mongodb::error::WriteFailure::WriteConcernError(wc) => {
                    (wc.code, wc.message.clone())
                }
                _ => (0, e.to_string()),
            };
            let kind = if code == 13 {
                ErrorKind::Permission { grant: None }
            } else {
                ErrorKind::Server
            };
            Error::new(kind, message.clone()).with_server(ServerMessage {
                code: Some(code.to_string()),
                message,
                position: None,
            })
        }
        K::Io(io) => match io.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
            Some(tls) => Error::new(
                ErrorKind::Tls,
                format!(
                    "The TLS connection to {host}:{port} failed: {tls}. Check the CA in the TLS \
                     settings or the server name."
                ),
            ),
            None => Error::network(host, port, io),
        },
        K::ServerSelection { message, .. } | K::DnsResolve { message, .. } => {
            if message.contains("certificate") || message.contains("TLS") || message.contains("tls")
            {
                Error::new(
                    ErrorKind::Tls,
                    format!("The TLS connection to {host}:{port} failed: {message}"),
                )
            } else if message.contains("Authentication") || message.contains("auth") {
                Error::new(ErrorKind::Auth, message.clone())
            } else {
                Error::new(
                    ErrorKind::Network {
                        host: host.to_owned(),
                        port,
                        cause: NetworkCause::Refused,
                    },
                    format!("{host}:{port} could not be reached: {message}"),
                )
            }
        }
        K::InvalidTlsConfig { message, .. } => Error::new(ErrorKind::Tls, message.clone()),
        _ => Error::protocol(e.to_string()),
    }
}
