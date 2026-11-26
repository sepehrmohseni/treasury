//! Every fixture here is synthetic and built in code, with each client's own encryption.

use super::*;
use crate::crypto::BEEKEEPER_DEFAULT_KEY;
use crate::crypto::testing::{dbeaver_encrypt, simple_encrypt};
use secrecy::ExposeSecret;
use serde_json::json;
use std::io::Write;
use treasury_core::config::{SshAuth, TlsMode};

fn secret(imported: &Imported, slot: SecretSlot) -> Option<String> {
    imported
        .secrets
        .iter()
        .find(|(s, _)| *s == slot)
        .map(|(_, v)| v.expose_secret().to_owned())
}

fn by_name<'a>(r: &'a Report, name: &str) -> &'a Imported {
    r.imported
        .iter()
        .find(|i| i.config.name == name)
        .unwrap_or_else(|| panic!("{name} not imported"))
}

fn dbeaver_sources() -> serde_json::Value {
    json!({
        "folders": { "work": {} },
        "connections": {
            "postgres-jdbc-1": {
                "provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders prod",
                "save-password": true, "folder": "work", "read-only": true,
                "configuration": {
                    "host": "db.internal", "port": "6543", "database": "orders", "type": "prod",
                    "auth-model": "native",
                    "handlers": {
                        "ssh_tunnel": {
                            "enabled": true, "save-password": true,
                            "properties": {
                                "host": "bastion.internal", "port": 2222, "authType": "PUBLIC_KEY",
                                "keyPath": "/home/me/.ssh/id_ed25519",
                                "jumpServer0.enabled": "true", "jumpServer0.host": "gw.example.com",
                                "jumpServer0.port": "22", "jumpServer0.name": "jumper",
                                "jumpServer0.authType": "PASSWORD"
                            }
                        },
                        "proxy": { "enabled": true, "properties": {} }
                    }
                }
            },
            "mysql8-2": {
                "provider": "mysql", "driver": "mariaDB", "name": "Shop dev",
                "save-password": true,
                "configuration": { "host": "localhost", "port": 3307, "database": "shop", "type": "dev" }
            },
            "snowflake-3": {
                "provider": "generic", "driver": "snowflake", "name": "Warehouse",
                "configuration": { "url": "jdbc:snowflake://acme-xy.snowflakecomputing.com/?warehouse=WH&db=SALES" }
            },
            "sqlite-4": {
                "provider": "generic", "driver": "sqlite_jdbc", "name": "Local file",
                "configuration": { "database": "/tmp/x.db" }
            }
        }
    })
}

fn dbeaver_credentials() -> Vec<u8> {
    let creds = json!({
        "postgres-jdbc-1": {
            "#connection": { "user": "app", "password": "pg-secret" },
            "network/ssh_tunnel": { "user": "tunnel", "password": "key-passphrase", "jumpServer0.password": "jump-secret" }
        },
        "mysql8-2": { "#connection": { "user": "root", "password": "my-secret" } }
    });
    dbeaver_encrypt(creds.to_string().as_bytes())
}

fn dbeaver_zip(prefix: &str) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("meta.xml", opts).unwrap();
    zip.write_all(b"<archive/>").unwrap();
    zip.start_file(format!("{prefix}/.dbeaver/data-sources.json"), opts)
        .unwrap();
    zip.write_all(dbeaver_sources().to_string().as_bytes())
        .unwrap();
    zip.start_file(format!("{prefix}/.dbeaver/credentials-config.json"), opts)
        .unwrap();
    zip.write_all(&dbeaver_credentials()).unwrap();
    zip.finish().unwrap().into_inner()
}

fn check_dbeaver(r: &Report) {
    assert_eq!(r.imported.len(), 3);
    assert_eq!(r.skipped.len(), 1);
    assert!(r.skipped[0].reason.contains("sqlite"));

    let pg = by_name(r, "Orders prod");
    assert_eq!(pg.config.engine, Engine::Postgres);
    assert_eq!(
        pg.config.endpoint,
        Endpoint::Host {
            host: "db.internal".into(),
            port: 6543
        }
    );
    assert_eq!(pg.config.environment, Environment::Production);
    assert_eq!(pg.config.user.as_deref(), Some("app"));
    assert_eq!(pg.config.folder.as_deref(), Some("work"));
    assert!(pg.config.read_only);
    assert_eq!(
        secret(pg, SecretSlot::Password).as_deref(),
        Some("pg-secret")
    );
    let ssh = pg.config.ssh.as_ref().unwrap();
    assert_eq!(ssh.target.host, "bastion.internal");
    assert_eq!(ssh.target.port, 2222);
    assert_eq!(ssh.target.user, "tunnel");
    assert!(matches!(ssh.target.auth, SshAuth::KeyFile { .. }));
    assert_eq!(
        secret(pg, SecretSlot::SshKeyPassphrase).as_deref(),
        Some("key-passphrase")
    );
    let jump = ssh.jump.as_ref().unwrap();
    assert_eq!(
        (jump.host.as_str(), jump.user.as_str()),
        ("gw.example.com", "jumper")
    );
    assert_eq!(
        secret(pg, SecretSlot::JumpPassword).as_deref(),
        Some("jump-secret")
    );
    assert!(pg.notes.iter().any(|n| n.contains("proxy")));

    let my = by_name(r, "Shop dev");
    assert_eq!(my.config.engine, Engine::MariaDb);
    assert_eq!(my.config.environment, Environment::Local);
    assert_eq!(
        secret(my, SecretSlot::Password).as_deref(),
        Some("my-secret")
    );

    let sf = by_name(r, "Warehouse");
    assert_eq!(sf.config.engine, Engine::Snowflake);
    assert_eq!(
        sf.config.endpoint,
        Endpoint::Account {
            account: "acme-xy".into()
        }
    );
    assert_eq!(
        sf.config.options.get("warehouse").map(String::as_str),
        Some("WH")
    );
}

#[test]
fn dbeaver_project_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("General.dbp");
    std::fs::write(&path, dbeaver_zip("projects/General")).unwrap();
    assert_eq!(detect(&path), Some(Source::DBeaver));
    check_dbeaver(&import(&path, Source::DBeaver).unwrap());
}

#[test]
fn dbeaver_export_zipped_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut outer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    outer
        .start_file("General.dbp", zip::write::SimpleFileOptions::default())
        .unwrap();
    outer.write_all(&dbeaver_zip("General")).unwrap();
    let bytes = outer.finish().unwrap().into_inner();
    let path = dir.path().join("dbp.zip");
    std::fs::write(&path, bytes).unwrap();
    check_dbeaver(&import(&path, Source::DBeaver).unwrap());
}

#[test]
fn dbeaver_workspace_folder_and_json_file() {
    let dir = tempfile::tempdir().unwrap();
    let dbeaver = dir
        .path()
        .join("workspace6")
        .join("General")
        .join(".dbeaver");
    std::fs::create_dir_all(&dbeaver).unwrap();
    std::fs::write(
        dbeaver.join("data-sources.json"),
        dbeaver_sources().to_string(),
    )
    .unwrap();
    std::fs::write(
        dbeaver.join("credentials-config.json"),
        dbeaver_credentials(),
    )
    .unwrap();
    assert_eq!(detect(dir.path()), Some(Source::DBeaver));
    check_dbeaver(&import(dir.path(), Source::DBeaver).unwrap());
    check_dbeaver(&import(&dbeaver.join("data-sources.json"), Source::DBeaver).unwrap());
}

#[test]
fn dbeaver_without_credentials_imports_without_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data-sources.json");
    std::fs::write(&path, dbeaver_sources().to_string()).unwrap();
    let r = import(&path, Source::DBeaver).unwrap();
    let pg = by_name(&r, "Orders prod");
    assert!(pg.secrets.is_empty());
    assert!(
        pg.notes
            .iter()
            .any(|n| n.contains("credentials-config.json"))
    );
}

#[test]
fn broken_dbeaver_input_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.dbp");
    std::fs::write(&path, b"not a zip").unwrap();
    assert!(import(&path, Source::DBeaver).is_err());
    let empty = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()))
        .finish()
        .unwrap()
        .into_inner();
    std::fs::write(&path, empty).unwrap();
    assert!(import(&path, Source::DBeaver).is_err());
}

#[test]
fn beekeeper_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    std::fs::write(
        dir.path().join(".key"),
        simple_encrypt(BEEKEEPER_DEFAULT_KEY, &json!({ "encryptionKey": key })),
    )
    .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("app.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE connection_folder (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO connection_folder VALUES (1, 'Clients');
         CREATE TABLE saved_connection (id INTEGER PRIMARY KEY, name TEXT, connectionType TEXT, host TEXT,
           port INTEGER, username TEXT, defaultDatabase TEXT, password TEXT, url TEXT, ssl INTEGER,
           sslRejectUnauthorized INTEGER, sshEnabled INTEGER, sshHost TEXT, sshPort INTEGER,
           sshUsername TEXT, sshMode TEXT, sshPassword TEXT, readOnlyMode INTEGER,
           connectionFolderId INTEGER, labelColor TEXT, snowflakeOptions TEXT);",
    )
    .unwrap();
    let enc = |s: &str| simple_encrypt(key, &json!(s));
    db.execute(
        "INSERT INTO saved_connection (name, connectionType, host, port, username, defaultDatabase, password, ssl,
           sslRejectUnauthorized, sshEnabled, sshHost, sshPort, sshUsername, sshMode, sshPassword, readOnlyMode,
           connectionFolderId, labelColor) VALUES ('Billing', 'postgresql', 'pg.example.com', 5433, 'billing',
           'billing', ?1, 1, 0, 1, 'jump.example.com', 22, 'ops', 'userpass', ?2, 1, 1, 'red')",
        [enc("pg-pass"), enc("ssh-pass")],
    )
    .unwrap();
    db.execute(
        "INSERT INTO saved_connection (name, connectionType, url) VALUES ('Mongo', 'mongodb', ?1)",
        [enc("mongodb://reader:mongo-pass@m1.example.com:27017/app")],
    )
    .unwrap();
    db.execute(
        "INSERT INTO saved_connection (name, connectionType, host) VALUES ('Lite', 'sqlite', 'x')",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(detect(dir.path()), Some(Source::Beekeeper));
    let r = import(dir.path(), Source::Beekeeper).unwrap();
    assert_eq!(r.imported.len(), 2);
    assert_eq!(r.skipped.len(), 1);
    let pg = by_name(&r, "Billing");
    assert_eq!(
        pg.config.endpoint,
        Endpoint::Host {
            host: "pg.example.com".into(),
            port: 5433
        }
    );
    assert_eq!(pg.config.folder.as_deref(), Some("Clients"));
    assert!(pg.config.read_only);
    assert_eq!(pg.config.tls.mode, TlsMode::Require);
    assert!(pg.config.tls.accept_invalid);
    assert_eq!(pg.config.color.as_deref(), Some("#c0392b"));
    assert_eq!(secret(pg, SecretSlot::Password).as_deref(), Some("pg-pass"));
    assert_eq!(
        secret(pg, SecretSlot::SshPassword).as_deref(),
        Some("ssh-pass")
    );
    assert!(matches!(
        pg.config.ssh.as_ref().unwrap().target.auth,
        SshAuth::Password
    ));
    let mongo = by_name(&r, "Mongo");
    assert_eq!(mongo.config.engine, Engine::MongoDb);
    assert_eq!(
        secret(mongo, SecretSlot::Password).as_deref(),
        Some("mongo-pass")
    );
    assert!(matches!(&mongo.config.endpoint, Endpoint::Uri { uri } if !uri.contains("mongo-pass")));
}

#[test]
fn beekeeper_with_a_wrong_key_file_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".key"), "garbage").unwrap();
    std::fs::write(dir.path().join("app.db"), "").unwrap();
    let Err(err) = import(dir.path(), Source::Beekeeper) else {
        panic!("a damaged .key file must fail");
    };
    assert!(err.message.contains(".key"));
}

#[test]
fn pgadmin_servers_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("servers.json");
    std::fs::write(
        &path,
        json!({ "Servers": {
            "1": { "Name": "Reporting", "Group": "Analytics", "Host": "pg.example.com", "Port": 5432,
                   "MaintenanceDB": "postgres", "Username": "analyst", "SSLMode": "verify-full",
                   "UseSSHTunnel": 1, "TunnelHost": "bastion", "TunnelPort": "2200",
                   "TunnelUsername": "me", "TunnelAuthentication": 1, "TunnelIdentityFile": "/k/id",
                   "BGColor": "#AA0000" },
            "2": { "Name": "Local", "Group": "Servers", "Host": "localhost", "Port": 5432,
                   "MaintenanceDB": "postgres", "Username": "postgres",
                   "ConnectionParameters": { "sslmode": "disable" } }
        }})
        .to_string(),
    )
    .unwrap();
    assert_eq!(detect(&path), Some(Source::PgAdmin));
    let r = import(&path, Source::PgAdmin).unwrap();
    let rep = by_name(&r, "Reporting");
    assert_eq!(rep.config.folder.as_deref(), Some("Analytics"));
    assert_eq!(rep.config.tls.mode, TlsMode::Require);
    assert_eq!(rep.config.color.as_deref(), Some("#aa0000"));
    let ssh = rep.config.ssh.as_ref().unwrap();
    assert_eq!((ssh.target.port, ssh.target.user.as_str()), (2200, "me"));
    let local = by_name(&r, "Local");
    assert_eq!(local.config.environment, Environment::Local);
    assert_eq!(local.config.tls.mode, TlsMode::Disable);
    assert_eq!(local.config.folder, None);
}

#[test]
fn jetbrains_data_sources() {
    let dir = tempfile::tempdir().unwrap();
    let idea = dir.path().join(".idea");
    std::fs::create_dir_all(&idea).unwrap();
    std::fs::write(
        idea.join("dataSources.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="orders@db" uuid="u1">
      <driver-ref>postgresql</driver-ref>
      <jdbc-driver>org.postgresql.Driver</jdbc-driver>
      <jdbc-url>jdbc:postgresql://db.example.com:5432/orders?application_name=a&amp;sslmode=require</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="cache" uuid="u2">
      <driver-ref>redis</driver-ref>
      <jdbc-url>jdbc:redis://cache.example.com:6380/0</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="duck" uuid="u3">
      <driver-ref>duckdb</driver-ref>
      <jdbc-url>jdbc:duckdb:/tmp/x</jdbc-url>
    </data-source>
  </component>
</project>"#,
    )
    .unwrap();
    std::fs::write(
        idea.join("dataSources.local.xml"),
        r#"<project version="4"><component name="dataSourceStorageLocal">
  <data-source name="orders@db" uuid="u1"><user-name>reporter</user-name></data-source>
</component></project>"#,
    )
    .unwrap();
    assert_eq!(detect(dir.path()), Some(Source::JetBrains));
    let r = import(dir.path(), Source::JetBrains).unwrap();
    assert_eq!(r.imported.len(), 2);
    assert_eq!(r.skipped.len(), 1);
    let pg = by_name(&r, "orders@db");
    assert_eq!(pg.config.user.as_deref(), Some("reporter"));
    assert_eq!(pg.config.database.as_deref(), Some("orders"));
    assert!(pg.config.tls.accept_invalid, "sslmode after &amp; was read");
    let cache = by_name(&r, "cache");
    assert_eq!(cache.config.engine, Engine::Redis);
    assert_eq!(
        cache.config.endpoint,
        Endpoint::Host {
            host: "cache.example.com".into(),
            port: 6380
        }
    );
}

#[test]
fn compass_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compass.json");
    std::fs::write(
        &path,
        json!({ "type": "Compass Connections", "version": { "$numberInt": "1" }, "connections": [
            { "id": "a", "connectionOptions": { "connectionString": "mongodb+srv://ana@cluster0.example.net/app" },
              "favorite": { "name": "Atlas", "color": "color1" } },
            { "id": "b", "connectionOptions": { "connectionString": "mongodb://localhost:27017" } }
        ]})
        .to_string(),
    )
    .unwrap();
    assert_eq!(detect(&path), Some(Source::Compass));
    let r = import(&path, Source::Compass).unwrap();
    assert_eq!(r.imported.len(), 2);
    let atlas = by_name(&r, "Atlas");
    assert_eq!(atlas.config.user.as_deref(), Some("ana"));
    assert!(atlas.secrets.is_empty());
    assert!(!atlas.notes.is_empty());
}

#[test]
fn every_imported_config_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("General.dbp");
    std::fs::write(&path, dbeaver_zip("p")).unwrap();
    for i in import(&path, Source::DBeaver).unwrap().imported {
        i.config.validate().unwrap();
    }
}
