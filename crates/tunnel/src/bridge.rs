//! A TLS bridge: a loopback listener whose connections are forwarded to a server over TLS made
//! with Treasury's own configuration.
//!
//! Some client libraries build their own TLS and cannot verify a certificate against a name other
//! than the address they dial, or cannot use the platform's verifier, or pull in another crypto
//! provider. They connect in plain TCP to the bridge instead; the bridge opens TLS to the server
//! (directly or through an SSH tunnel's local port) and checks the certificate against the
//! configured name. Like an SSH local forward, the listener accepts only loopback connections.

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use treasury_core::{Error, Result};

/// A running bridge. Dropping it stops accepting connections; open ones end with their peers.
pub struct TlsBridge {
    local: SocketAddr,
    task: JoinHandle<()>,
}

impl TlsBridge {
    /// Listens on a free loopback port and forwards each connection to `upstream` over TLS,
    /// verifying the certificate for `name` with `config`.
    pub async fn start(
        upstream: SocketAddr,
        config: Arc<ClientConfig>,
        name: ServerName<'static>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::internal(format!("a local port for the TLS bridge: {e}")))?;
        let local = listener
            .local_addr()
            .map_err(|e| Error::internal(format!("the TLS bridge's address: {e}")))?;
        let connector = TlsConnector::from(config);
        let task = tokio::spawn(async move {
            while let Ok((mut inbound, peer)) = listener.accept().await {
                if !peer.ip().is_loopback() {
                    continue;
                }
                let connector = connector.clone();
                let name = name.clone();
                tokio::spawn(async move {
                    let Ok(tcp) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _ = tcp.set_nodelay(true);
                    match connector.connect(name, tcp).await {
                        Ok(mut tls) => {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut tls).await;
                        }
                        Err(e) => tracing::debug!("TLS bridge handshake failed: {e}"),
                    }
                });
            }
        });
        Ok(Self { local, task })
    }

    /// The loopback address clients connect to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Checks that a TLS connection to the server can be made, so certificate problems surface
    /// as TLS errors rather than as the client library's lost connection.
    pub async fn probe(
        upstream: SocketAddr,
        config: Arc<ClientConfig>,
        name: ServerName<'static>,
        host: &str,
        port: u16,
    ) -> Result<()> {
        let tcp = TcpStream::connect(upstream)
            .await
            .map_err(|e| Error::network(host, port, &e))?;
        match TlsConnector::from(config).connect(name, tcp).await {
            Ok(_) => Ok(()),
            Err(e) => Err(
                match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                    Some(rustls::Error::InvalidCertificate(why)) => Error::new(
                        treasury_core::ErrorKind::Tls,
                        format!(
                            "The certificate of {host}:{port} is not trusted ({why:?}). Add the CA that \
                         issued it in the TLS settings, check the server name, or turn on \"Accept \
                         invalid certificates\" for this connection."
                        ),
                    ),
                    Some(other) => Error::new(
                        treasury_core::ErrorKind::Tls,
                        format!("The TLS handshake with {host}:{port} failed: {other}."),
                    ),
                    None => Error::network(host, port, &e),
                },
            ),
        }
    }
}

impl Drop for TlsBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}
