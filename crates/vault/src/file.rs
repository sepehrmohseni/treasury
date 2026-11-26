//! The fallback secret store: one file sealed under a passphrase the user chooses, for systems
//! with no keychain or Secret Service (headless Linux, minimal desktops).

use crate::crypto::{Envelope, KdfParams};
use crate::{Backend, SecretStore, VaultError};
use secrecy::{ExposeSecret, SecretString};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use zeroize::Zeroizing;

/// An encrypted file of secrets, held decrypted in memory while unlocked.
pub struct FileVault {
    path: PathBuf,
    passphrase: SecretString,
    kdf: KdfParams,
    entries: Mutex<BTreeMap<String, Zeroizing<String>>>,
}

impl FileVault {
    /// Opens the vault at `path` with `passphrase`, creating an empty one if the file does not
    /// exist yet.
    pub fn open(path: &Path, passphrase: SecretString) -> Result<Self, VaultError> {
        Self::open_with(path, passphrase, KdfParams::DEFAULT)
    }

    pub(crate) fn open_with(
        path: &Path,
        passphrase: SecretString,
        kdf: KdfParams,
    ) -> Result<Self, VaultError> {
        let entries = match std::fs::read(path) {
            Ok(data) => {
                let plain = Envelope::open(&passphrase, &data)?;
                rmp_serde::from_slice::<BTreeMap<String, String>>(&plain)
                    .map_err(|_| VaultError::Format("the vault file is damaged"))?
                    .into_iter()
                    .map(|(k, v)| (k, Zeroizing::new(v)))
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(VaultError::Io(e.to_string())),
        };
        Ok(Self {
            path: path.to_owned(),
            passphrase,
            kdf,
            entries: Mutex::new(entries),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Zeroizing<String>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the vault: to a temporary file in the same folder, flushed, then renamed over the
    /// old one, so a crash mid-write never leaves a half-written vault.
    fn persist(&self, entries: &BTreeMap<String, Zeroizing<String>>) -> Result<(), VaultError> {
        let plain: BTreeMap<&str, &str> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let bytes = Zeroizing::new(
            rmp_serde::to_vec(&plain).map_err(|_| VaultError::Format("could not encode"))?,
        );
        let sealed = Envelope::seal_with(&self.passphrase, &bytes, self.kdf)?;
        let dir = self
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| VaultError::Io(e.to_string()))?;
        let mut tmp =
            tempfile::NamedTempFile::new_in(dir).map_err(|e| VaultError::Io(e.to_string()))?;
        tmp.write_all(&sealed)
            .and_then(|()| tmp.as_file().sync_all())
            .map_err(|e| VaultError::Io(e.to_string()))?;
        // tempfile creates the file readable by the owner only (0600 on Unix).
        tmp.persist(&self.path)
            .map_err(|e| VaultError::Io(e.error.to_string()))?;
        Ok(())
    }

    /// Re-seals the vault under a new passphrase.
    pub fn change_passphrase(&mut self, new: SecretString) -> Result<(), VaultError> {
        if new.expose_secret().is_empty() {
            return Err(VaultError::EmptyPassphrase);
        }
        self.passphrase = new;
        let entries = self.lock().clone();
        self.persist(&entries)
    }
}

impl SecretStore for FileVault {
    fn get(&self, key: &str) -> Result<Option<SecretString>, VaultError> {
        Ok(self
            .lock()
            .get(key)
            .map(|v| SecretString::from(v.as_str().to_owned())))
    }

    fn set(&self, key: &str, value: &SecretString) -> Result<(), VaultError> {
        let mut entries = self.lock();
        entries.insert(
            key.to_owned(),
            Zeroizing::new(value.expose_secret().to_owned()),
        );
        self.persist(&entries)
    }

    fn delete(&self, key: &str) -> Result<(), VaultError> {
        let mut entries = self.lock();
        if entries.remove(key).is_some() {
            self.persist(&entries)?;
        }
        Ok(())
    }

    fn backend(&self) -> Backend {
        Backend::File
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: KdfParams = KdfParams {
        m_kib: 8 * 1024,
        t: 1,
        p: 1,
    };

    fn pw(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn secrets_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let v = FileVault::open_with(&path, pw("pass"), FAST).unwrap();
        v.set("connection/a/password", &pw("hunter2")).unwrap();
        v.set("connection/b/password", &pw("swordfish")).unwrap();
        v.delete("connection/b/password").unwrap();
        v.delete("never-set").unwrap();
        drop(v);
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(7).any(|w| w == b"hunter2"));
        let v = FileVault::open_with(&path, pw("pass"), FAST).unwrap();
        assert_eq!(
            v.get("connection/a/password")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "hunter2"
        );
        assert!(v.get("connection/b/password").unwrap().is_none());
        assert!(matches!(
            FileVault::open_with(&path, pw("nope"), FAST),
            Err(VaultError::Decrypt)
        ));
    }

    #[test]
    fn changing_the_passphrase_re_seals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let mut v = FileVault::open_with(&path, pw("old"), FAST).unwrap();
        v.set("k", &pw("v")).unwrap();
        v.change_passphrase(pw("new")).unwrap();
        assert!(v.change_passphrase(pw("")).is_err());
        drop(v);
        assert!(FileVault::open_with(&path, pw("old"), FAST).is_err());
        let v = FileVault::open_with(&path, pw("new"), FAST).unwrap();
        assert_eq!(v.get("k").unwrap().unwrap().expose_secret(), "v");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let v = FileVault::open_with(&path, pw("p"), FAST).unwrap();
        v.set("k", &pw("v")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(FileVault::open_with(&path, pw("p"), FAST).is_err());
    }
}
