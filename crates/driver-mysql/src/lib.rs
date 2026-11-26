//! The MySQL and MariaDB driver, on mysql_async with rustls.
//!
//! Every statement is prepared first: preparing proves the text is a single statement (the
//! guardrails decided about one statement, and the protocol would otherwise run several), and the
//! binary protocol returns typed values. The few commands MySQL cannot prepare run through the
//! text protocol after Treasury's own splitter confirms they are one statement. Rows stream
//! through a channel holding one batch, so a slow reader stops the socket and the server waits.

#![forbid(unsafe_code)]

mod edit;
mod error;
mod explain;
mod health;
mod introspect;
mod session;
mod tls;
mod value;

pub use session::MySqlSession;

use async_trait::async_trait;
use std::sync::Arc;
use treasury_core::Result;
use treasury_core::config::{ConnectionConfig, Engine};
use treasury_core::driver::{ConnectContext, Driver, Session};

/// The driver for MySQL or MariaDB.
#[derive(Clone, Copy)]
pub struct MySqlDriver {
    engine: Engine,
}

impl MySqlDriver {
    /// The MySQL driver.
    pub fn mysql() -> Self {
        Self {
            engine: Engine::MySql,
        }
    }

    /// The MariaDB driver.
    pub fn mariadb() -> Self {
        Self {
            engine: Engine::MariaDb,
        }
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn engine(&self) -> Engine {
        self.engine
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>> {
        Ok(Arc::new(session::connect(self.engine, config, ctx).await?))
    }
}
