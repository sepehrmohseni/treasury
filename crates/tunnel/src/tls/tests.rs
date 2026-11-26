use super::*;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use treasury_core::config::TlsMode;

struct Pki {
    dir: tempfile::TempDir,
    ca_pem: String,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    client_cert: PathBuf,
    client_key: PathBuf,
    ca_file: PathBuf,
}

fn ca(name: &str) -> (Issuer<'static, KeyPair>, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let pem = cert.pem();
    (Issuer::new(params, key), pem)
}

fn pki() -> Pki {
    let dir = tempfile::tempdir().unwrap();
    let (issuer, ca_pem) = ca("Treasury test CA");
    let server_key = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&server_key, &issuer)
        .unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut cparams = CertificateParams::new(Vec::<String>::new()).unwrap();
    cparams
        .distinguished_name
        .push(DnType::CommonName, "app-user");
    let client = cparams.signed_by(&client_key, &issuer).unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, &ca_pem).unwrap();
    let client_cert = dir.path().join("client.pem");
    std::fs::write(&client_cert, client.pem()).unwrap();
    let client_key_path = dir.path().join("client.key");
    std::fs::write(&client_key_path, client_key.serialize_pem()).unwrap();
    Pki {
        server_chain: vec![server.der().clone()],
        server_key: PrivateKeyDer::try_from(server_key.serialize_der()).unwrap(),
        client_cert,
        client_key: client_key_path,
        ca_file,
        ca_pem,
        dir,
    }
}

/// Serves one TLS connection that echoes a line; requires a client certificate signed by the CA
/// when `require_client` is set. Returns the address.
async fn serve(pki: &Pki, require_client: bool) -> std::net::SocketAddr {
    let provider = provider();
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = if require_client {
        let mut roots = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(pki.ca_pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        let v =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .unwrap();
        builder.with_client_cert_verifier(v)
    } else {
        builder.with_no_client_auth()
    };
    let config = builder
        .with_single_cert(pki.server_chain.clone(), pki.server_key.clone_key())
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((tcp, _)) = listener.accept().await
            && let Ok(mut tls) = acceptor.accept(tcp).await
        {
            let mut buf = [0u8; 5];
            if tls.read_exact(&mut buf).await.is_ok() {
                let _ = tls.write_all(&buf).await;
            }
        }
    });
    addr
}

async fn talk(
    addr: std::net::SocketAddr,
    tls: &TlsConfig,
    name: &str,
) -> std::result::Result<(), String> {
    let config = client_config(tls, None).map_err(|e| e.to_string())?;
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let mut stream = TlsConnector::from(config)
        .connect(server_name(tls, name).map_err(|e| e.to_string())?, tcp)
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"hello")
        .await
        .map_err(|e| e.to_string())?;
    let mut back = [0u8; 5];
    stream
        .read_exact(&mut back)
        .await
        .map_err(|e| e.to_string())?;
    assert_eq!(&back, b"hello");
    Ok(())
}

fn with_ca(pki: &Pki) -> TlsConfig {
    TlsConfig {
        mode: TlsMode::Require,
        ca_file: Some(pki.ca_file.clone()),
        ..TlsConfig::default()
    }
}

#[tokio::test]
async fn custom_ca_verifies_the_server() {
    let pki = pki();
    let addr = serve(&pki, false).await;
    talk(addr, &with_ca(&pki), "localhost").await.unwrap();
}

#[tokio::test]
async fn the_wrong_name_is_refused() {
    let pki = pki();
    let addr = serve(&pki, false).await;
    assert!(talk(addr, &with_ca(&pki), "db.example.com").await.is_err());
}

#[tokio::test]
async fn a_different_ca_is_refused() {
    let pki = pki();
    let other = self::pki();
    let addr = serve(&pki, false).await;
    assert!(talk(addr, &with_ca(&other), "localhost").await.is_err());
}

#[tokio::test]
async fn the_system_store_does_not_trust_a_private_ca() {
    let pki = pki();
    let addr = serve(&pki, false).await;
    assert!(
        talk(addr, &TlsConfig::default(), "localhost")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn accept_invalid_connects_anyway() {
    let pki = pki();
    let addr = serve(&pki, false).await;
    let tls = TlsConfig {
        accept_invalid: true,
        ..TlsConfig::default()
    };
    talk(addr, &tls, "anything.invalid").await.unwrap();
}

#[tokio::test]
async fn client_certificates_are_presented() {
    let pki = pki();
    let addr = serve(&pki, true).await;
    let mut tls = with_ca(&pki);
    tls.client_cert = Some(pki.client_cert.clone());
    tls.client_key = Some(pki.client_key.clone());
    talk(addr, &tls, "localhost").await.unwrap();
    let addr = serve(&pki, true).await;
    // TLS 1.3 reports a missing client certificate after the handshake, on first read.
    assert!(talk(addr, &with_ca(&pki), "localhost").await.is_err());
}

#[test]
fn encrypted_keys_need_their_passphrase() {
    let pki = pki();
    let plain = std::fs::read_to_string(&pki.client_key).unwrap();
    let (_, der) = pkcs8::der::pem::decode_vec(plain.trim().as_bytes()).unwrap();
    let info = pkcs8::PrivateKeyInfoRef::try_from(der.as_slice()).unwrap();
    let encrypted = info
        .encrypt_with_rng(&mut rand::rng(), b"open sesame")
        .unwrap();
    let pem = encrypted
        .to_pem("ENCRYPTED PRIVATE KEY", pkcs8::LineEnding::LF)
        .unwrap();
    let path = pki.dir.path().join("enc.key");
    std::fs::write(&path, pem.as_bytes()).unwrap();
    assert!(load_private_key(&path, None).is_err());
    let wrong = SecretString::from("nope".to_owned());
    assert_eq!(
        load_private_key(&path, Some(&wrong)).unwrap_err().kind,
        ErrorKind::Auth
    );
    let right = SecretString::from("open sesame".to_owned());
    let key = load_private_key(&path, Some(&right)).unwrap();
    assert_eq!(key.secret_der(), der.as_slice());
    assert!(load_private_key(&pki.client_key, None).is_ok());
}

#[test]
fn missing_and_empty_files_are_clear_errors() {
    let pki = pki();
    let tls = TlsConfig {
        ca_file: Some(pki.dir.path().join("missing.pem")),
        ..TlsConfig::default()
    };
    assert!(client_config(&tls, None).is_err());
    let empty = pki.dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let tls = TlsConfig {
        ca_file: Some(empty),
        ..TlsConfig::default()
    };
    assert!(matches!(
        client_config(&tls, None).unwrap_err().kind,
        ErrorKind::Config { .. }
    ));
}
