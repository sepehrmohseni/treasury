//! The SQL Server driver, on tiberius (the maintained tiberius-ng line) with rustls on ring.
//!
//! Rows stream from the TDS token stream through a channel holding one batch. Cancel sends the
//! protocol's Attention signal. SQL Server has no session-wide read-only mode, so Treasury's
//! guardrails enforce read-only and block statements they cannot classify.

#![forbid(unsafe_code)]

mod edit;
mod error;
mod explain;
mod health;
mod introspect;
mod session;
mod value;

pub use session::MsSqlSession;

use async_trait::async_trait;
use std::sync::Arc;
use treasury_core::Result;
use treasury_core::config::{ConnectionConfig, Engine};
use treasury_core::driver::{ConnectContext, Driver, Session};

/// The SQL Server driver.
#[derive(Default, Clone, Copy)]
pub struct MsSqlDriver;

#[async_trait]
impl Driver for MsSqlDriver {
    fn engine(&self) -> Engine {
        Engine::SqlServer
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>> {
        Ok(Arc::new(session::connect(config, ctx).await?))
    }
}
