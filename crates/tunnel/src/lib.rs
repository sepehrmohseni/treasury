//! Network plumbing shared by the drivers: SSH tunnels and TLS client configuration.

#![forbid(unsafe_code)]

pub mod known_hosts;
pub mod ssh;
pub mod tls;

pub use known_hosts::{HostKeyPrompt, HostKeyStatus, KnownHosts, fingerprint};
pub use ssh::{Tunnel, TunnelParams, TunnelState};
