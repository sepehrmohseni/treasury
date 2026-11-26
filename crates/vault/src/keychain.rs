//! The platform credential stores through keyring-core: Keychain on macOS and iOS, Credential
//! Manager on Windows, Secret Service on Linux and the BSDs.

use crate::{Backend, SecretStore, VaultError};
use keyring_core::{CredentialStore, Entry, Error as KeyringError};
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;

/// The service name Treasury's entries are filed under.
pub const SERVICE: &str = "io.github.sepehrmohseni.treasury";

/// A [`SecretStore`] backed by a keyring-core credential store.
pub struct Keychain {
    store: Arc<CredentialStore>,
    backend: Backend,
}

impl Keychain {
    /// Wraps an existing credential store (the platform store, or keyring-core's mock in tests).
    pub fn new(store: Arc<CredentialStore>, backend: Backend) -> Self {
        Self { store, backend }
    }

    /// The platform's store, or an error saying why there is none.
    pub fn platform() -> Result<Self, VaultError> {
        #[cfg(target_os = "macos")]
        let store: Arc<CredentialStore> =
            apple_native_keyring_store::keychain::Store::new().map_err(map)?;
        #[cfg(target_os = "ios")]
        let store: Arc<CredentialStore> =
            apple_native_keyring_store::protected::Store::new().map_err(map)?;
        #[cfg(target_os = "windows")]
        let store: Arc<CredentialStore> =
            windows_native_keyring_store::Store::new().map_err(map)?;
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "ios", target_os = "android"))
        ))]
        let store: Arc<CredentialStore> =
            zbus_secret_service_keyring_store::Store::new().map_err(map)?;
        #[cfg(any(target_os = "android", not(any(unix, windows))))]
        return Err(VaultError::Unavailable(
            "this platform's secret store is provided by the app".into(),
        ));
        #[cfg(not(any(target_os = "android", not(any(unix, windows)))))]
        Ok(Self::new(store, Backend::Platform))
    }

    fn entry(&self, key: &str) -> Result<Entry, VaultError> {
        Entry::new_in_store(&self.store, SERVICE, key).map_err(map)
    }
}

fn map(e: KeyringError) -> VaultError {
    match e {
        KeyringError::NoStorageAccess(_) => VaultError::Locked,
        KeyringError::NoDefaultStore => VaultError::Unavailable("no default store".into()),
        // Platform error text can name the entry but never contains the secret itself.
        other => VaultError::Platform(other.to_string()),
    }
}

impl SecretStore for Keychain {
    fn get(&self, key: &str) -> Result<Option<SecretString>, VaultError> {
        match self.entry(key)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(e) => Err(map(e)),
        }
    }

    fn set(&self, key: &str, value: &SecretString) -> Result<(), VaultError> {
        self.entry(key)?
            .set_password(value.expose_secret())
            .map_err(map)
    }

    fn delete(&self, key: &str) -> Result<(), VaultError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(map(e)),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_entries_are_none_and_deletes_are_idempotent() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let k = Keychain::new(store, Backend::Platform);
        assert!(k.get("connection/x/password").unwrap().is_none());
        k.delete("connection/x/password").unwrap();
        assert_eq!(k.backend(), Backend::Platform);
    }

    #[test]
    fn set_then_get_on_one_entry() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let k = Keychain::new(store, Backend::Platform);
        let entry = k.entry("connection/y/password").unwrap();
        entry.set_password("s3cret").unwrap();
        assert_eq!(entry.get_password().unwrap(), "s3cret");
    }
}
