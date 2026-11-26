//! TLS client configuration shared by every driver: rustls with the ring provider, the system's
//! trust store through rustls-platform-verifier, or a CA the user supplies, and optional client
//! certificates.

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use std::sync::Arc;
use treasury_core::config::TlsConfig;
use treasury_core::{Error, ErrorKind, Result};

/// The single crypto provider of the workspace.
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Tls, message)
}

/// The rustls client configuration for `tls`. `key_passphrase` unlocks an encrypted client key.
pub fn client_config(
    tls: &TlsConfig,
    key_passphrase: Option<&SecretString>,
) -> Result<Arc<ClientConfig>> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_error(format!("TLS could not be configured: {e}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = if tls.accept_invalid {
        Arc::new(AcceptAnyCertificate {
            algorithms: provider.signature_verification_algorithms,
        })
    } else if let Some(ca) = &tls.ca_file {
        let mut roots = RootCertStore::empty();
        for cert in read_certs(ca)? {
            roots.add(cert).map_err(|e| {
                tls_error(format!(
                    "The CA certificate in {} is not usable: {e}.",
                    ca.display()
                ))
            })?;
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|e| {
                tls_error(format!(
                    "The CA certificate in {} is not usable: {e}.",
                    ca.display()
                ))
            })?
    } else {
        Arc::new(
            rustls_platform_verifier::Verifier::new(provider.clone()).map_err(|e| {
                tls_error(format!(
                    "The system certificate store is not available: {e}."
                ))
            })?,
        )
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => {
            let chain = read_certs(cert)?;
            let key = load_private_key(key, key_passphrase)?;
            builder.with_client_auth_cert(chain, key).map_err(|e| {
                tls_error(format!("The client certificate and key do not match: {e}."))
            })?
        }
        _ => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// The name TLS verifies: the override if set, else the host.
pub fn server_name(tls: &TlsConfig, host: &str) -> Result<ServerName<'static>> {
    let name = tls.server_name.as_deref().unwrap_or(host);
    ServerName::try_from(name.to_owned()).map_err(|_| {
        Error::config(
            Some("tls"),
            format!("\"{name}\" is not a valid TLS server name."),
        )
    })
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<_> = CertificateDer::pem_file_iter(path)
        .map_err(|e| {
            Error::config(
                Some("tls"),
                format!("{} could not be read: {e}.", path.display()),
            )
        })?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| {
            Error::config(
                Some("tls"),
                format!("{} is not a PEM certificate file: {e}.", path.display()),
            )
        })?;
    if certs.is_empty() {
        return Err(Error::config(
            Some("tls"),
            format!("{} holds no certificates.", path.display()),
        ));
    }
    Ok(certs)
}

/// Loads a PEM private key: PKCS#8, PKCS#1 or SEC1, or an encrypted PKCS#8 key unlocked with
/// `passphrase`. Snowflake key-pair authentication uses the same files.
pub fn load_private_key(
    path: &Path,
    passphrase: Option<&SecretString>,
) -> Result<PrivateKeyDer<'static>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::config(
            Some("key"),
            format!("{} could not be read: {e}.", path.display()),
        )
    })?;
    if text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        let Some(passphrase) = passphrase else {
            return Err(Error::config(
                Some("key"),
                format!("{} is encrypted. Enter its passphrase.", path.display()),
            ));
        };
        let (label, der) = pkcs8::der::pem::decode_vec(text.trim().as_bytes()).map_err(|_| {
            Error::config(
                Some("key"),
                format!("{} is not a valid PEM key.", path.display()),
            )
        })?;
        if label != "ENCRYPTED PRIVATE KEY" {
            return Err(Error::config(
                Some("key"),
                format!("{} is not a valid PEM key.", path.display()),
            ));
        }
        let info = pkcs8::EncryptedPrivateKeyInfoRef::try_from(der.as_slice()).map_err(|_| {
            Error::config(
                Some("key"),
                format!("{} is not a valid encrypted key.", path.display()),
            )
        })?;
        let doc = info
            .decrypt(passphrase.expose_secret().as_bytes())
            .map_err(|_| {
                Error::new(
                    ErrorKind::Auth,
                    format!("The passphrase for {} is wrong.", path.display()),
                )
            })?;
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            doc.as_bytes().to_vec(),
        )));
    }
    PrivateKeyDer::from_pem_slice(text.as_bytes()).map_err(|e| {
        Error::config(
            Some("key"),
            format!("{} holds no usable private key: {e}.", path.display()),
        )
    })
}

/// Accepts any certificate, for the explicit "Accept invalid certificates" setting. Handshake
/// signatures are still checked, so the connection is encrypted to whoever holds the presented
/// key; it is just not known who that is.
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests;
