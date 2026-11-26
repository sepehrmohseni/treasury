//! TLS settings for mysql_async, from the connection's TLS configuration.
//!
//! mysql_async builds its own rustls configuration, so the trust decisions every engine shares
//! are expressed through its options: a custom CA is the only one trusted; otherwise the
//! system's roots are added to the bundled ones; the certificate is checked against the
//! configured host (or the override), not the tunnel's local address.

use mysql_async::{ClientIdentity, SslOpts};
use rustls::pki_types::PrivateKeyDer;
use secrecy::SecretString;
use std::sync::OnceLock;
use treasury_core::Result;
use treasury_core::config::TlsConfig;

pub(crate) fn ssl_opts(
    tls: &TlsConfig,
    host: &str,
    key_passphrase: Option<&SecretString>,
) -> Result<SslOpts> {
    let name = tls.server_name.clone().unwrap_or_else(|| host.to_owned());
    let mut ssl = SslOpts::default().with_danger_tls_hostname_override(Some(name));
    ssl = match &tls.ca_file {
        Some(ca) => ssl
            .with_root_certs(vec![ca.clone().into()])
            .with_disable_built_in_roots(true),
        None => ssl.with_root_certs(system_roots().iter().map(|c| c.clone().into()).collect()),
    };
    if tls.accept_invalid {
        ssl = ssl.with_danger_accept_invalid_certs(true);
    }
    if let (Some(cert), Some(key)) = (&tls.client_cert, &tls.client_key) {
        let key = treasury_tunnel::tls::load_private_key(key, key_passphrase)?;
        ssl = ssl.with_client_identity(Some(ClientIdentity::new(
            cert.clone().into(),
            pem(&key).into_bytes().into(),
        )));
    }
    Ok(ssl)
}

/// The operating system's trusted roots (DER), loaded once.
fn system_roots() -> &'static [Vec<u8>] {
    static ROOTS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            rustls_native_certs::load_native_certs()
                .certs
                .into_iter()
                .map(|c| c.to_vec())
                .collect()
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            Vec::new()
        }
    })
}

/// The key as PEM: mysql_async reads raw DER only as PKCS#1, so PKCS#8 and SEC1 keys (also the
/// ones decrypted with a passphrase) go as PEM text.
fn pem(key: &PrivateKeyDer<'_>) -> String {
    let (label, der) = match key {
        PrivateKeyDer::Pkcs1(k) => ("RSA PRIVATE KEY", k.secret_pkcs1_der()),
        PrivateKeyDer::Sec1(k) => ("EC PRIVATE KEY", k.secret_sec1_der()),
        _ => ("PRIVATE KEY", key.secret_der()),
    };
    let body = base64(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::PrivatePkcs8KeyDer;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn keys_become_pem_that_rustls_reads_back() {
        let der: Vec<u8> = (0..200u8).collect();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der.clone()));
        let text = pem(&key);
        assert!(text.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(text.lines().all(|l| l.len() <= 64));
        use rustls::pki_types::pem::PemObject;
        let back = PrivateKeyDer::from_pem_slice(text.as_bytes()).unwrap();
        assert_eq!(back.secret_der(), der.as_slice());
    }
}
