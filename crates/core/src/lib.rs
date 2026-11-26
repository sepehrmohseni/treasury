//! The core of Treasury: the value model, connection settings, the driver capability traits,
//! errors, result cursors and the page cache. It has no driver dependencies; drivers depend on it.

#![forbid(unsafe_code)]

pub mod analyze;
#[cfg(feature = "runtime")]
pub mod cache;
pub mod config;
#[cfg(feature = "runtime")]
pub mod cursor;
pub mod driver;
pub mod edit;
pub mod error;
pub mod exec;
pub mod format;
pub mod guard;
pub mod health;
pub mod json;
pub mod plan;
pub mod schema;
#[cfg(feature = "runtime")]
pub mod task;
pub mod uri;
pub mod value;

pub use error::{Error, ErrorKind, Result};
pub use value::Value;
