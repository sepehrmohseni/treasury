//! The MongoDB driver, on the official driver with rustls (ring).
//!
//! The editor language is database commands in Extended JSON. Results stream from server
//! cursors; their columns are the top-level fields of the first batch plus one column that keeps
//! any field later documents add, so nothing is lost. MongoDB has no read-only session, so the
//! guardrails enforce read-only from the command table.

#![forbid(unsafe_code)]

mod edit;
mod error;
mod explain;
mod health;
mod introspect;
pub mod lang;
mod session;
mod value;

pub use lang::MongoAnalyzer;
pub use session::MongoSession;

use async_trait::async_trait;
use std::sync::Arc;
use treasury_core::Result;
use treasury_core::config::{ConnectionConfig, Engine};
use treasury_core::driver::{ConnectContext, Driver, Session};

/// The MongoDB driver.
#[derive(Default, Clone, Copy)]
pub struct MongoDriver;

#[async_trait]
impl Driver for MongoDriver {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        ctx: ConnectContext,
    ) -> Result<Arc<dyn Session>> {
        Ok(Arc::new(session::connect(config, ctx).await?))
    }
}
