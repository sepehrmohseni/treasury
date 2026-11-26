//! Where Treasury keeps secrets: the platform keychain where there is one, otherwise a file
//! sealed under a passphrase. Secrets are `secrecy` types throughout; nothing here prints them.

#![forbid(unsafe_code)]

pub mod crypto;
mod file;
mod keychain;

pub use crypto::{Envelope, KdfParams, Sealer, random_key};
pub use file::FileVault;
pub use keychain::{Keychain, SERVICE};

use secrecy::SecretString;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use treasury_core::config::{ConnectionId, SecretSlot};
use treasury_core::driver::SecretSource;
use treasury_core::{Error, ErrorKind};

/// Which kind of store holds the secrets, for the settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The operating system's keychain or secret service.
    Platform,
    /// Treasury's own encrypted file.
    File,
    /// Memory only (tests, or a session the user chose not to persist).
    Memory,
}

/// Errors from secret storage. None of them carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// The platform store exists but refused access (locked keychain, denied prompt).
    #[error("The system keychain is locked or refused access.")]
    Locked,
    /// There is no platform store.
    #[error("No system keychain is available: {0}.")]
    Unavailable(String),
    /// The platform store reported an error.
    #[error("The system keychain reported an error: {0}")]
    Platform(String),
    /// Wrong passphrase, or the data was altered.
    #[error("The passphrase is wrong, or the file was changed after it was written.")]
    Decrypt,
    /// The data is not in the expected format.
    #[error("The file cannot be read: {0}.")]
    Format(&'static str),
    /// A passphrase was empty.
    #[error("Enter a passphrase.")]
    EmptyPassphrase,
    /// The OS had no randomness to give.
    #[error("The system could not provide random numbers.")]
    Random,
    /// Reading or writing the vault file failed.
    #[error("Could not read or write the vault file: {0}")]
    Io(String),
}

impl From<VaultError> for Error {
    fn from(e: VaultError) -> Self {
        let kind = match e {
            VaultError::Decrypt | VaultError::EmptyPassphrase => ErrorKind::Auth,
            VaultError::Format(_) => ErrorKind::Config { field: None },
            _ => ErrorKind::Internal,
        };
        Error::new(kind, e.to_string())
    }
}

/// A place secrets can be read from and written to, by key.
pub trait SecretStore: Send + Sync {
    /// The secret stored under `key`, if any.
    fn get(&self, key: &str) -> Result<Option<SecretString>, VaultError>;
    /// Stores `value` under `key`, replacing what was there.
    fn set(&self, key: &str, value: &SecretString) -> Result<(), VaultError>;
    /// Removes `key`. Removing a missing key is not an error.
    fn delete(&self, key: &str) -> Result<(), VaultError>;
    /// The kind of store.
    fn backend(&self) -> Backend;

    /// Whether `key` is set. This is all the UI ever learns about a secret.
    fn is_set(&self, key: &str) -> Result<bool, VaultError> {
        Ok(self.get(key)?.is_some())
    }
}

/// Secrets held in memory only.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<BTreeMap<String, SecretString>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<SecretString>, VaultError> {
        let map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(map.get(key).cloned())
    }

    fn set(&self, key: &str, value: &SecretString) -> Result<(), VaultError> {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(key.to_owned(), value.clone());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), VaultError> {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        map.remove(key);
        Ok(())
    }

    fn backend(&self) -> Backend {
        Backend::Memory
    }
}

/// One connection's secrets, as the drivers see them.
pub struct ConnectionSecrets {
    store: Arc<dyn SecretStore>,
    connection: ConnectionId,
}

impl ConnectionSecrets {
    /// The secrets of `connection` in `store`.
    pub fn new(store: Arc<dyn SecretStore>, connection: ConnectionId) -> Self {
        Self { store, connection }
    }

    /// Stores a secret for `slot`.
    pub fn set(&self, slot: SecretSlot, value: &SecretString) -> treasury_core::Result<()> {
        Ok(self.store.set(&slot.key(&self.connection), value)?)
    }

    /// Removes every slot of the connection, for when it is deleted.
    pub fn clear(&self) -> treasury_core::Result<()> {
        for slot in ALL_SLOTS {
            self.store.delete(&slot.key(&self.connection))?;
        }
        Ok(())
    }

    /// Which slots are set, for the connection form.
    pub fn set_slots(&self) -> treasury_core::Result<Vec<SecretSlot>> {
        let mut out = Vec::new();
        for slot in ALL_SLOTS {
            if self.store.is_set(&slot.key(&self.connection))? {
                out.push(slot);
            }
        }
        Ok(out)
    }
}

const ALL_SLOTS: [SecretSlot; 9] = [
    SecretSlot::Password,
    SecretSlot::Token,
    SecretSlot::ClientSecret,
    SecretSlot::RefreshToken,
    SecretSlot::KeyPassphrase,
    SecretSlot::SshPassword,
    SecretSlot::SshKeyPassphrase,
    SecretSlot::JumpPassword,
    SecretSlot::JumpKeyPassphrase,
];

impl SecretSource for ConnectionSecrets {
    fn secret(&self, slot: SecretSlot) -> treasury_core::Result<Option<SecretString>> {
        Ok(self.store.get(&slot.key(&self.connection))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn connection_secrets_are_scoped() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let a = ConnectionSecrets::new(store.clone(), ConnectionId("a".into()));
        let b = ConnectionSecrets::new(store.clone(), ConnectionId("b".into()));
        a.set(SecretSlot::Password, &SecretString::from("pa".to_owned()))
            .unwrap();
        b.set(
            SecretSlot::SshPassword,
            &SecretString::from("sb".to_owned()),
        )
        .unwrap();
        assert_eq!(
            a.secret(SecretSlot::Password)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "pa"
        );
        assert!(a.secret(SecretSlot::SshPassword).unwrap().is_none());
        assert_eq!(a.set_slots().unwrap(), vec![SecretSlot::Password]);
        a.clear().unwrap();
        assert!(a.set_slots().unwrap().is_empty());
        assert_eq!(b.set_slots().unwrap(), vec![SecretSlot::SshPassword]);
    }

    #[test]
    fn errors_never_contain_secrets_and_map_to_core_kinds() {
        let e: Error = VaultError::Decrypt.into();
        assert_eq!(e.kind, ErrorKind::Auth);
        let debug = format!("{:?}", SecretString::from("hunter2".to_owned()));
        assert!(!debug.contains("hunter2"));
    }
}
