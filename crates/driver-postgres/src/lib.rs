//! The PostgreSQL driver, on tokio-postgres with rustls.
//!
//! Results stream through the protocol's unnamed portal. tokio-postgres hands responses over a
//! channel that holds one message, so when the reader stops, the connection stops reading the
//! socket and the server pauses: memory stays bounded however large the result. Statements
//! without parameters use the simple protocol, whose text format renders every type, extension
//! types included, exactly as PostgreSQL writes it.

#![forbid(unsafe_code)]

mod binary;
mod edit;
mod error;
mod explain;
mod health;
mod introspect;
mod session;
mod text;
mod tls;

pub use session::PgSession;

use async_trait::async_trait;
use secrecy::ExposeSecret;
use std::sync::Arc;
use tokio_postgres::config::SslMode;
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, SecretSlot, TlsMode};
use treasury_core::driver::{ConnectContext, Driver, Session};
use treasury_core::{Error, Result};

/// Settings every session starts with, so values have one spelling and timestamps one zone.
const SESSION_OPTIONS: &str = "-c DateStyle=ISO -c IntervalStyle=iso_8601 -c TimeZone=UTC \
    -c bytea_output=hex -c extra_float_digits=1";

/// The PostgreSQL driver.
#[derive(Default, Clone, Copy)]
pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>> {
        let session = connect(config, ctx).await?;
        Ok(Arc::new(session))
    }
}

async fn connect(config: &ConnectionConfig, ctx: ConnectContext) -> Result<PgSession> {
    let Endpoint::Host { host, port } = &config.endpoint else {
        return Err(Error::config(
            Some("host"),
            "PostgreSQL connections need a host and port.",
        ));
    };
    let (dial_host, dial_port) = match ctx.via {
        Some(addr) => (addr.ip().to_string(), addr.port()),
        None => (host.clone(), *port),
    };
    let mut pg = tokio_postgres::Config::new();
    pg.host(&dial_host)
        .port(dial_port)
        .application_name("Treasury")
        .connect_timeout(ctx.connect_timeout)
        .keepalives(true)
        .options(SESSION_OPTIONS);
    pg.user(config.user.as_deref().unwrap_or("postgres"));
    pg.dbname(config.database.as_deref().unwrap_or("postgres"));
    if let Some(pw) = ctx.secrets.secret(SecretSlot::Password)? {
        pg.password(pw.expose_secret());
    }
    pg.ssl_mode(match config.tls.mode {
        TlsMode::Disable => SslMode::Disable,
        TlsMode::Prefer => SslMode::Prefer,
        TlsMode::Require => SslMode::Require,
    });
    let passphrase = ctx.secrets.secret(SecretSlot::KeyPassphrase)?;
    let rustls = treasury_tunnel::tls::client_config(&config.tls, passphrase.as_ref())?;
    let name = treasury_tunnel::tls::server_name(&config.tls, host)?;
    let tls = tls::MakeRustls::new(rustls, name);
    let connecting = pg.connect(tls.clone());
    let (client, connection) = match tokio::time::timeout(ctx.connect_timeout, connecting).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(error::map(&e, host, *port)),
        Err(_) => return Err(Error::timeout(ctx.connect_timeout)),
    };
    let session = PgSession::start(
        client,
        connection,
        tls,
        host.clone(),
        *port,
        ctx.metadata_timeout,
    );
    if ctx.read_only {
        use treasury_core::driver::Session as _;
        session.set_read_only(true).await?;
    }
    Ok(session)
}
