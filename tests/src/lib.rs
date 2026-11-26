//! Cross-crate test support: the driver conformance suite every driver runs, an in-memory mock
//! driver that keeps the suite itself honest, and helpers for tests that need Docker.
//!
//! Tests that start containers live in modules named `docker`, so `cargo xtask ci test` can
//! leave them out and `cargo xtask ci integration` can run only them.

#![forbid(unsafe_code)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout
)]

pub mod conformance;
pub mod docker;
pub mod memory;
pub mod mock;

use secrecy::SecretString;
use std::collections::HashMap;
use treasury_core::Result;
use treasury_core::config::SecretSlot;
use treasury_core::driver::SecretSource;

/// Secrets held in a map, for tests.
#[derive(Default, Clone)]
pub struct MapSecrets(pub HashMap<SecretSlot, String>);

impl MapSecrets {
    /// Secrets with only a database password.
    pub fn password(p: &str) -> Self {
        let mut m = HashMap::new();
        m.insert(SecretSlot::Password, p.to_owned());
        Self(m)
    }
}

impl SecretSource for MapSecrets {
    fn secret(&self, slot: SecretSlot) -> Result<Option<SecretString>> {
        Ok(self.0.get(&slot).map(|s| SecretString::from(s.clone())))
    }
}
