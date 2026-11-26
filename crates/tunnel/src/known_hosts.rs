//! SSH host key verification against OpenSSH `known_hosts` files.
//!
//! Treasury reads the user's `~/.ssh/known_hosts` but never writes it; keys the user accepts in
//! Treasury go to Treasury's own file. A key that differs from a recorded one is refused, with no
//! prompt: that is what an attack looks like.

use async_trait::async_trait;
use russh::keys::{HashAlg, PublicKey};
use std::path::PathBuf;
use treasury_core::{Error, ErrorKind, Result};

/// What a known_hosts lookup found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is recorded for this host.
    Known,
    /// Nothing is recorded for this host.
    Unknown,
    /// A different key is recorded for this host.
    Changed,
}

/// Asks the user whether to trust a host key seen for the first time.
#[async_trait]
pub trait HostKeyPrompt: Send + Sync {
    /// Shows `fingerprint` (SHA-256, as OpenSSH prints it) for `host:port` and returns whether
    /// the user accepted it.
    async fn confirm_new(&self, host: &str, port: u16, algorithm: &str, fingerprint: &str) -> bool;
}

/// The known_hosts files Treasury consults.
#[derive(Clone, Debug)]
pub struct KnownHosts {
    /// Treasury's own file, which accepted keys are added to.
    pub app_file: PathBuf,
    /// The user's OpenSSH file, read only.
    pub user_file: Option<PathBuf>,
}

/// The SHA-256 fingerprint of `key` as OpenSSH prints it (`SHA256:...`).
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

impl KnownHosts {
    /// Treasury's file in `app_dir`, plus `~/.ssh/known_hosts` when there is a home directory.
    pub fn new(app_file: PathBuf) -> Self {
        let user_file = std::env::home_dir().map(|h| h.join(".ssh").join("known_hosts"));
        Self {
            app_file,
            user_file,
        }
    }

    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.app_file).chain(self.user_file.iter())
    }

    /// Looks `key` up for `host:port` in every file. A match anywhere is known; a different key
    /// recorded anywhere is a change.
    pub fn status(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let mut changed = false;
        for file in self.files() {
            if !file.exists() {
                continue;
            }
            match russh::keys::check_known_hosts_path(host, port, key, file) {
                Ok(true) => return HostKeyStatus::Known,
                Ok(false) => {
                    if russh::keys::known_hosts::known_host_keys_path(host, port, file)
                        .map(|keys| !keys.is_empty())
                        .unwrap_or(false)
                    {
                        changed = true;
                    }
                }
                Err(russh::keys::Error::KeyChanged { .. }) => changed = true,
                // An unreadable line in the user's own file is not a reason to refuse.
                Err(_) => {}
            }
        }
        if changed {
            HostKeyStatus::Changed
        } else {
            HostKeyStatus::Unknown
        }
    }

    /// Records `key` for `host:port` in Treasury's file.
    pub fn learn(&self, host: &str, port: u16, key: &PublicKey) -> Result<()> {
        if let Some(dir) = self.app_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                Error::internal(format!("Could not create {}: {e}.", dir.display()))
            })?;
        }
        russh::keys::known_hosts::learn_known_hosts_path(host, port, key, &self.app_file)
            .map_err(|e| Error::internal(format!("Could not record the host key: {e}.")))
    }

    /// Checks `key`, asking `prompt` about unknown keys and recording accepted ones.
    pub async fn verify(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
        prompt: &dyn HostKeyPrompt,
    ) -> Result<()> {
        let print = fingerprint(key);
        match self.status(host, port, key) {
            HostKeyStatus::Known => Ok(()),
            HostKeyStatus::Changed => Err(Error::new(
                ErrorKind::SshHostKey {
                    host: host.to_owned(),
                    fingerprint: print.clone(),
                    changed: true,
                },
                format!(
                    "The SSH host key of {host} has changed since it was last recorded. This can \
                     mean someone is intercepting the connection. If the server was reinstalled, \
                     remove its old key from known_hosts and connect again. New fingerprint: {print}."
                ),
            )),
            HostKeyStatus::Unknown => {
                if prompt
                    .confirm_new(host, port, key.algorithm().as_str(), &print)
                    .await
                {
                    self.learn(host, port, key)
                } else {
                    Err(Error::new(
                        ErrorKind::SshHostKey {
                            host: host.to_owned(),
                            fingerprint: print,
                            changed: false,
                        },
                        format!("The SSH host key of {host} was not accepted."),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};

    struct Answer(bool);

    #[async_trait]
    impl HostKeyPrompt for Answer {
        async fn confirm_new(&self, _: &str, _: u16, _: &str, fp: &str) -> bool {
            assert!(fp.starts_with("SHA256:"));
            self.0
        }
    }

    fn key() -> PublicKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .clone()
    }

    fn hosts(dir: &tempfile::TempDir) -> KnownHosts {
        KnownHosts {
            app_file: dir.path().join("treasury_known_hosts"),
            user_file: Some(dir.path().join("user_known_hosts")),
        }
    }

    #[tokio::test]
    async fn accepted_keys_are_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let kh = hosts(&dir);
        let k = key();
        assert_eq!(kh.status("bastion", 22, &k), HostKeyStatus::Unknown);
        kh.verify("bastion", 22, &k, &Answer(true)).await.unwrap();
        assert_eq!(kh.status("bastion", 22, &k), HostKeyStatus::Known);
        // Accepted now, no prompt needed (a prompt answering no would fail otherwise).
        kh.verify("bastion", 22, &k, &Answer(false)).await.unwrap();
        // Ports are part of the identity.
        assert_eq!(kh.status("bastion", 2222, &k), HostKeyStatus::Unknown);
    }

    #[tokio::test]
    async fn declined_and_changed_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let kh = hosts(&dir);
        let k = key();
        let err = kh
            .verify("bastion", 22, &k, &Answer(false))
            .await
            .unwrap_err();
        assert!(matches!(
            err.kind,
            ErrorKind::SshHostKey { changed: false, .. }
        ));
        kh.learn("bastion", 22, &k).unwrap();
        let other = key();
        assert_eq!(kh.status("bastion", 22, &other), HostKeyStatus::Changed);
        let err = kh
            .verify("bastion", 22, &other, &Answer(true))
            .await
            .unwrap_err();
        assert!(matches!(
            err.kind,
            ErrorKind::SshHostKey { changed: true, .. }
        ));
    }

    #[tokio::test]
    async fn the_users_file_is_read_but_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let kh = hosts(&dir);
        let k = key();
        let user = kh.user_file.clone().unwrap();
        russh::keys::known_hosts::learn_known_hosts_path("db-gw", 22, &k, &user).unwrap();
        let before = std::fs::read(&user).unwrap();
        assert_eq!(kh.status("db-gw", 22, &k), HostKeyStatus::Known);
        kh.verify("other", 22, &k, &Answer(true)).await.unwrap();
        assert_eq!(std::fs::read(&user).unwrap(), before);
    }
}
