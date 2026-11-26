//! Network plumbing shared by the drivers: SSH tunnels, TLS client configuration, and TLS bridges
//! for clients that cannot take Treasury's TLS configuration.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod known_hosts;
pub mod ssh;
pub mod tls;

pub use bridge::TlsBridge;
pub use known_hosts::{HostKeyPrompt, HostKeyStatus, KnownHosts, fingerprint};
pub use ssh::{Tunnel, TunnelParams, TunnelState};
