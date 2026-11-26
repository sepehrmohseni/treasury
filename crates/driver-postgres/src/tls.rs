//! rustls for tokio-postgres. The server name to verify comes from the connection settings, not
//! from the address tokio-postgres dials, so a connection through an SSH tunnel (127.0.0.1)
//! still checks the database's certificate against its real host name.

use futures::future::BoxFuture;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Makes TLS connections for tokio-postgres, including the cancel connection.
#[derive(Clone)]
pub struct MakeRustls {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl MakeRustls {
    /// A connector verifying `name` with `config`.
    pub fn new(config: Arc<ClientConfig>, name: ServerName<'static>) -> Self {
        Self { config, name }
    }
}

impl<S> MakeTlsConnect<S> for MakeRustls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, _domain: &str) -> io::Result<RustlsConnect> {
        Ok(RustlsConnect {
            connector: TlsConnector::from(self.config.clone()),
            name: self.name.clone(),
        })
    }
}

/// One TLS handshake.
pub struct RustlsConnect {
    connector: TlsConnector,
    name: ServerName<'static>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<RustlsStream<S>>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let tls = self.connector.connect(self.name, stream).await?;
            Ok(RustlsStream(tls))
        })
    }
}

/// A TLS stream tokio-postgres can use.
pub struct RustlsStream<S>(TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> tokio_postgres::tls::TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        // SCRAM channel binding needs the server certificate's hash with the algorithm of its
        // signature; without it tokio-postgres uses plain SCRAM-SHA-256.
        ChannelBinding::none()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
