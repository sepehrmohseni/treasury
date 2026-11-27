//! The Redis and Valkey driver, on redis-rs with RESP3.
//!
//! The editor language is commands as redis-cli takes them, one per line. A single server is
//! reached directly or through an SSH tunnel, with TLS made by Treasury's own bridge; with
//! Sentinel, the master the Sentinels name; a cluster, through its nodes, each command going to
//! the node that holds its key. Redis has no read-only session, so the guardrails
//! enforce read-only from the ACL categories the server gives each command.

#![forbid(unsafe_code)]

mod conn;
mod error;
mod introspect;
pub mod lang;
mod session;
mod value;

pub use lang::RedisAnalyzer;
pub use session::RedisSession;

use async_trait::async_trait;
use std::sync::Arc;
use treasury_core::Result;
use treasury_core::config::{ConnectionConfig, Engine};
use treasury_core::driver::{ConnectContext, Driver, Session};

/// The Redis driver, for Redis or Valkey.
#[derive(Clone, Copy)]
pub struct RedisDriver {
    engine: Engine,
}

impl RedisDriver {
    /// The driver for Redis.
    pub fn redis() -> Self {
        Self {
            engine: Engine::Redis,
        }
    }

    /// The driver for Valkey.
    pub fn valkey() -> Self {
        Self {
            engine: Engine::Valkey,
        }
    }
}

#[async_trait]
impl Driver for RedisDriver {
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
