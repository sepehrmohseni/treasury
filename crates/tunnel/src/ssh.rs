//! SSH tunnels: a local listener on 127.0.0.1 whose connections are forwarded through an SSH
//! host (optionally reached through one jump host) to the database.

use crate::known_hosts::{HostKeyPrompt, KnownHosts};
use russh::client::{self, Handle, Handler};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use secrecy::{ExposeSecret, SecretString};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use treasury_core::config::{SecretSlot, SshAuth, SshConfig, SshHop};
use treasury_core::driver::SecretSource;
use treasury_core::{Error, ErrorKind, Result};

/// Whether the tunnel is still usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelState {
    /// Forwarding connections.
    Open,
    /// The SSH connection ended; the message says why.
    Closed(String),
}

/// What a tunnel needs to open.
pub struct TunnelParams<'a> {
    /// The SSH hops.
    pub ssh: &'a SshConfig,
    /// The database host and port, as the SSH host sees them.
    pub target: (&'a str, u16),
    /// Passwords and passphrases.
    pub secrets: &'a dyn SecretSource,
    /// Where host keys are checked and recorded.
    pub known_hosts: &'a KnownHosts,
    /// Asks the user about keys seen for the first time.
    pub prompt: Arc<dyn HostKeyPrompt>,
    /// Time limit for each step of connecting.
    pub timeout: Duration,
}

/// An open tunnel. Dropping it closes the listener and the SSH connections.
pub struct Tunnel {
    local: SocketAddr,
    state: watch::Receiver<TunnelState>,
    accept: JoinHandle<()>,
    _sessions: Vec<Arc<Handle<Client>>>,
}

impl Tunnel {
    /// Connects the SSH hops and starts listening on an ephemeral port of 127.0.0.1.
    pub async fn open(p: TunnelParams<'_>) -> Result<Self> {
        let (state_tx, state_rx) = watch::channel(TunnelState::Open);
        let state_tx = Arc::new(state_tx);
        let mut sessions = Vec::new();
        let target_session = match &p.ssh.jump {
            None => {
                let stream = tcp(&p.ssh.target, p.timeout).await?;
                let mut s = handshake(stream, &p.ssh.target, &p, state_tx.clone()).await?;
                authenticate(&mut s, &p.ssh.target, p.secrets, false, p.timeout).await?;
                Arc::new(s)
            }
            Some(jump) => {
                let stream = tcp(jump, p.timeout).await?;
                let mut j = handshake(stream, jump, &p, state_tx.clone()).await?;
                authenticate(&mut j, jump, p.secrets, true, p.timeout).await?;
                let channel = timed(
                    p.timeout,
                    j.channel_open_direct_tcpip(
                        p.ssh.target.host.clone(),
                        u32::from(p.ssh.target.port),
                        "127.0.0.1",
                        0,
                    ),
                )
                .await?
                .map_err(|e| {
                    Error::new(
                        ErrorKind::Network {
                            host: p.ssh.target.host.clone(),
                            port: p.ssh.target.port,
                            cause: treasury_core::error::NetworkCause::Refused,
                        },
                        format!(
                            "The jump host {} could not reach {}:{}: {e}.",
                            jump.host, p.ssh.target.host, p.ssh.target.port
                        ),
                    )
                })?;
                let mut t =
                    handshake(channel.into_stream(), &p.ssh.target, &p, state_tx.clone()).await?;
                authenticate(&mut t, &p.ssh.target, p.secrets, false, p.timeout).await?;
                sessions.push(Arc::new(j));
                Arc::new(t)
            }
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
            Error::internal(format!("Could not open a local port for the tunnel: {e}."))
        })?;
        let local = listener.local_addr().map_err(|e| {
            Error::internal(format!("Could not open a local port for the tunnel: {e}."))
        })?;
        let (db_host, db_port) = (p.target.0.to_owned(), p.target.1);
        let session = target_session.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut local_conn, peer)) = listener.accept().await else {
                    break;
                };
                let session = session.clone();
                let host = db_host.clone();
                tokio::spawn(async move {
                    let _ = local_conn.set_nodelay(true);
                    let channel = match session
                        .channel_open_direct_tcpip(
                            host.clone(),
                            u32::from(db_port),
                            peer.ip().to_string(),
                            u32::from(peer.port()),
                        )
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::debug!(%host, port = db_port, "tunnel could not reach the database: {e}");
                            return;
                        }
                    };
                    let mut remote = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut local_conn, &mut remote).await;
                });
            }
        });
        sessions.push(target_session);
        Ok(Self {
            local,
            state: state_rx,
            accept,
            _sessions: sessions,
        })
    }

    /// The local address the driver connects to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Changes of state, for the connection's status in the UI.
    pub fn state(&self) -> watch::Receiver<TunnelState> {
        self.state.clone()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn timed<F: std::future::Future>(limit: Duration, f: F) -> Result<F::Output> {
    tokio::time::timeout(limit, f)
        .await
        .map_err(|_| Error::timeout(limit))
}

async fn tcp(hop: &SshHop, limit: Duration) -> Result<TcpStream> {
    let s = timed(limit, TcpStream::connect((hop.host.as_str(), hop.port)))
        .await?
        .map_err(|e| Error::network(&hop.host, hop.port, &e))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

fn ssh_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..client::Config::default()
    })
}

async fn handshake<S>(
    stream: S,
    hop: &SshHop,
    p: &TunnelParams<'_>,
    state: Arc<watch::Sender<TunnelState>>,
) -> Result<Handle<Client>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let failure = Arc::new(Mutex::new(None));
    let client = Client {
        host: hop.host.clone(),
        port: hop.port,
        known_hosts: p.known_hosts.clone(),
        prompt: p.prompt.clone(),
        failure: failure.clone(),
        state,
    };
    // The host key prompt waits for the user, so it is not bound by the connect timeout.
    match client::connect_stream(ssh_config(), stream, client).await {
        Ok(h) => Ok(h),
        Err(e) => {
            let recorded = failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            Err(recorded.unwrap_or_else(|| ssh_error(hop, &e)))
        }
    }
}

fn ssh_error(hop: &SshHop, e: &russh::Error) -> Error {
    match e {
        russh::Error::IO(io) => Error::network(&hop.host, hop.port, io),
        other => Error::protocol(format!(
            "The SSH connection to {} failed: {other}.",
            hop.host
        )),
    }
}

async fn authenticate(
    session: &mut Handle<Client>,
    hop: &SshHop,
    secrets: &dyn SecretSource,
    jump: bool,
    limit: Duration,
) -> Result<()> {
    let auth_failed = || {
        Error::new(
            ErrorKind::Auth,
            format!(
                "The SSH host {} did not accept the credentials for {}. Check the user and the \
                 password or key.",
                hop.host, hop.user
            ),
        )
    };
    let (password_slot, passphrase_slot) = if jump {
        (SecretSlot::JumpPassword, SecretSlot::JumpKeyPassphrase)
    } else {
        (SecretSlot::SshPassword, SecretSlot::SshKeyPassphrase)
    };
    let ok = match &hop.auth {
        SshAuth::Password => {
            let pw = secrets
                .secret(password_slot)?
                .unwrap_or_else(|| SecretString::from(String::new()));
            timed(
                limit,
                session.authenticate_password(hop.user.clone(), pw.expose_secret()),
            )
            .await?
            .map_err(|e| ssh_error(hop, &e))?
            .success()
        }
        SshAuth::KeyFile { path } => {
            let passphrase = secrets.secret(passphrase_slot)?;
            let key =
                russh::keys::load_secret_key(path, passphrase.as_ref().map(|p| p.expose_secret()))
                    .map_err(|e| match e {
                        russh::keys::Error::KeyIsEncrypted => Error::config(
                            Some("ssh"),
                            format!("{} is encrypted. Enter its passphrase.", path.display()),
                        ),
                        other => Error::config(
                            Some("ssh"),
                            format!("The SSH key {} could not be read: {other}.", path.display()),
                        ),
                    })?;
            let hash = timed(limit, session.best_supported_rsa_hash())
                .await?
                .map_err(|e| ssh_error(hop, &e))?
                .flatten();
            timed(
                limit,
                session.authenticate_publickey(
                    hop.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                ),
            )
            .await?
            .map_err(|e| ssh_error(hop, &e))?
            .success()
        }
        SshAuth::Agent => agent_auth(session, hop, limit).await?,
    };
    if ok { Ok(()) } else { Err(auth_failed()) }
}

#[cfg(unix)]
async fn connect_agent() -> Result<
    russh::keys::agent::client::AgentClient<
        Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>,
    >,
> {
    russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map(|a| a.dynamic())
        .map_err(|_| {
            Error::config(
                Some("ssh"),
                "No SSH agent is running (SSH_AUTH_SOCK is not set or not reachable).",
            )
        })
}

#[cfg(windows)]
async fn connect_agent() -> Result<
    russh::keys::agent::client::AgentClient<
        Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>,
    >,
> {
    russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map(|a| a.dynamic())
        .map_err(|_| Error::config(Some("ssh"), "The OpenSSH agent service is not running."))
}

async fn agent_auth(session: &mut Handle<Client>, hop: &SshHop, limit: Duration) -> Result<bool> {
    let mut agent = connect_agent().await?;
    let keys = agent.request_identities().await.map_err(|e| {
        Error::config(
            Some("ssh"),
            format!("The SSH agent did not list its keys: {e}."),
        )
    })?;
    if keys.is_empty() {
        return Err(Error::config(Some("ssh"), "The SSH agent holds no keys."));
    }
    let hash = timed(limit, session.best_supported_rsa_hash())
        .await?
        .map_err(|e| ssh_error(hop, &e))?
        .flatten();
    for identity in keys {
        let key = identity.public_key().into_owned();
        let r = timed(
            limit,
            session.authenticate_publickey_with(hop.user.clone(), key, hash, &mut agent),
        )
        .await?;
        if matches!(r, Ok(res) if res.success()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The russh handler: checks host keys and refuses every channel the server tries to open.
pub(crate) struct Client {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    prompt: Arc<dyn HostKeyPrompt>,
    failure: Arc<Mutex<Option<Error>>>,
    state: Arc<watch::Sender<TunnelState>>,
}

impl Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key: PublicKey = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match self
            .known_hosts
            .verify(&self.host, self.port, &key, self.prompt.as_ref())
            .await
        {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                Ok(false)
            }
        }
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> std::result::Result<(), Self::Error> {
        let message = match &reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                format!(
                    "The SSH host {} closed the connection: {}.",
                    self.host, info.message
                )
            }
            client::DisconnectReason::Error(e) => {
                format!("The SSH connection to {} was lost: {e}.", self.host)
            }
        };
        self.state.send_replace(TunnelState::Closed(message));
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}
