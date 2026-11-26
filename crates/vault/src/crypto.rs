//! Authenticated encryption for Treasury's secrets at rest.
//!
//! Two forms:
//! - [`Envelope`]: data sealed under a passphrase. Argon2id derives the key from the passphrase
//!   and a random salt; XChaCha20-Poly1305 seals the data. The header (format, KDF parameters,
//!   salt) is authenticated too, so it cannot be altered to weaken the KDF. Used by the vault file
//!   and by connection exports.
//! - [`Sealer`]: data sealed under a 32-byte key held elsewhere (the field encryption key of the
//!   local store, which lives in the SecretStore).

use crate::VaultError;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{AeadInOut, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"TRSV";
const FORMAT: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 4 + 1 + 12 + SALT_LEN;

/// Argon2id cost parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Iterations.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl KdfParams {
    /// The parameters new envelopes use: 64 MiB, 3 passes, 4 lanes (the RFC 9106 second
    /// recommendation, sized for desktops and phones alike).
    pub const DEFAULT: Self = Self {
        m_kib: 64 * 1024,
        t: 3,
        p: 4,
    };

    /// Rejects parameters a file could carry to make opening it slow or memory-hungry, or so
    /// weak that the passphrase would be cheap to guess.
    fn validate(self) -> Result<Self, VaultError> {
        let ok = (1..=16).contains(&self.p)
            && (1..=10).contains(&self.t)
            && self.m_kib >= 8 * 1024
            && self.m_kib <= 256 * 1024;
        if ok {
            Ok(self)
        } else {
            Err(VaultError::Format("unsupported key derivation parameters"))
        }
    }
}

fn random<const N: usize>() -> Result<[u8; N], VaultError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| VaultError::Random)?;
    Ok(b)
}

/// A new random 32-byte key.
pub fn random_key() -> Result<Zeroizing<[u8; 32]>, VaultError> {
    Ok(Zeroizing::new(random::<32>()?))
}

fn derive(
    passphrase: &SecretString,
    salt: &[u8],
    kdf: KdfParams,
) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let kdf = kdf.validate()?;
    let params = Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32))
        .map_err(|_| VaultError::Format("unsupported key derivation parameters"))?;
    let engine = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    engine
        .hash_password_into(
            passphrase.expose_secret().as_bytes(),
            salt,
            key.as_mut_slice(),
        )
        .map_err(|_| VaultError::Format("key derivation failed"))?;
    Ok(key)
}

fn seal_raw(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, VaultError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = random::<NONCE_LEN>()?;
    let mut out = Vec::with_capacity(NONCE_LEN + plain.len() + TAG_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(plain);
    let xnonce: &XNonce = (&nonce).into();
    let tag = cipher
        .encrypt_inout_detached(xnonce, aad, (&mut out[NONCE_LEN..]).into())
        .map_err(|_| VaultError::Format("data too large to seal"))?;
    let tag: [u8; TAG_LEN] = tag.into();
    out.extend_from_slice(&tag);
    Ok(out)
}

fn open_raw(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(VaultError::Format("the data is truncated"));
    }
    let (nonce, rest) = blob.split_at(NONCE_LEN);
    let (body, tag) = rest.split_at(rest.len() - TAG_LEN);
    let nonce: [u8; NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| VaultError::Format("the data is truncated"))?;
    let tag = Tag::try_from(tag).map_err(|_| VaultError::Format("the data is truncated"))?;
    let mut plain = Zeroizing::new(body.to_vec());
    let cipher = XChaCha20Poly1305::new(key.into());
    let xnonce: &XNonce = (&nonce).into();
    cipher
        .decrypt_inout_detached(xnonce, aad, plain.as_mut_slice().into(), &tag)
        .map_err(|_| VaultError::Decrypt)?;
    Ok(plain)
}

/// Data sealed under a passphrase.
pub struct Envelope;

impl Envelope {
    /// Seals `plain` under `passphrase` with the default KDF parameters.
    pub fn seal(passphrase: &SecretString, plain: &[u8]) -> Result<Vec<u8>, VaultError> {
        Self::seal_with(passphrase, plain, KdfParams::DEFAULT)
    }

    /// Seals `plain` under `passphrase` with explicit KDF parameters (tests use cheap ones).
    pub fn seal_with(
        passphrase: &SecretString,
        plain: &[u8],
        kdf: KdfParams,
    ) -> Result<Vec<u8>, VaultError> {
        if passphrase.expose_secret().is_empty() {
            return Err(VaultError::EmptyPassphrase);
        }
        let salt = random::<SALT_LEN>()?;
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(FORMAT);
        header.extend_from_slice(&kdf.m_kib.to_le_bytes());
        header.extend_from_slice(&kdf.t.to_le_bytes());
        header.extend_from_slice(&kdf.p.to_le_bytes());
        header.extend_from_slice(&salt);
        let key = derive(passphrase, &salt, kdf)?;
        let sealed = seal_raw(&key, &header, plain)?;
        header.extend_from_slice(&sealed);
        Ok(header)
    }

    /// Opens data sealed by [`Envelope::seal`]. A wrong passphrase and tampered data both give
    /// [`VaultError::Decrypt`].
    pub fn open(passphrase: &SecretString, data: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        if data.len() < HEADER_LEN || &data[..4] != MAGIC {
            return Err(VaultError::Format("this is not a Treasury encrypted file"));
        }
        if data[4] != FORMAT {
            return Err(VaultError::Format(
                "the file was written by a newer version of Treasury",
            ));
        }
        let word = |i: usize| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&data[i..i + 4]);
            u32::from_le_bytes(b)
        };
        let kdf = KdfParams {
            m_kib: word(5),
            t: word(9),
            p: word(13),
        };
        let (header, sealed) = data.split_at(HEADER_LEN);
        let salt = &header[17..];
        let key = derive(passphrase, salt, kdf)?;
        open_raw(&key, header, sealed)
    }

    /// Whether `data` starts like an envelope.
    pub fn looks_like(data: &[u8]) -> bool {
        data.len() >= HEADER_LEN && &data[..4] == MAGIC
    }
}

/// Seals and opens data under a fixed key.
pub struct Sealer {
    key: Zeroizing<[u8; 32]>,
}

impl Sealer {
    /// A sealer for `key`.
    pub fn new(key: Zeroizing<[u8; 32]>) -> Self {
        Self { key }
    }

    /// Seals `plain`; `context` (a table and column name, say) is authenticated so a value
    /// cannot be moved to another field unnoticed.
    pub fn seal(&self, context: &[u8], plain: &[u8]) -> Result<Vec<u8>, VaultError> {
        seal_raw(&self.key, context, plain)
    }

    /// Opens what [`Sealer::seal`] produced with the same `context`.
    pub fn open(&self, context: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        open_raw(&self.key, context, blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Cheap parameters so tests stay fast; production uses [`KdfParams::DEFAULT`].
    pub(crate) const FAST: KdfParams = KdfParams {
        m_kib: 8 * 1024,
        t: 1,
        p: 1,
    };

    fn pw(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn envelope_round_trip_and_wrong_passphrase() {
        let sealed = Envelope::seal_with(&pw("correct horse"), b"secret data", FAST).unwrap();
        assert!(Envelope::looks_like(&sealed));
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(
            &*Envelope::open(&pw("correct horse"), &sealed).unwrap(),
            b"secret data"
        );
        assert!(matches!(
            Envelope::open(&pw("wrong"), &sealed),
            Err(VaultError::Decrypt)
        ));
    }

    #[test]
    fn tampering_is_detected_anywhere() {
        let sealed = Envelope::seal_with(&pw("p"), b"payload", FAST).unwrap();
        for i in [5usize, 20, HEADER_LEN + 2, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(Envelope::open(&pw("p"), &bad).is_err(), "byte {i}");
        }
        assert!(Envelope::open(&pw("p"), &sealed[..sealed.len() - 3]).is_err());
        assert!(Envelope::open(&pw("p"), b"TRSV").is_err());
        assert!(Envelope::open(&pw("p"), b"not an envelope at all, no").is_err());
    }

    #[test]
    fn hostile_kdf_parameters_are_refused() {
        let mut sealed = Envelope::seal_with(&pw("p"), b"x", FAST).unwrap();
        // Ask for 4 TiB of memory.
        sealed[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Envelope::open(&pw("p"), &sealed),
            Err(VaultError::Format(_))
        ));
        assert!(matches!(
            Envelope::seal_with(
                &pw("p"),
                b"x",
                KdfParams {
                    m_kib: 1,
                    t: 1,
                    p: 1
                }
            ),
            Err(VaultError::Format(_))
        ));
    }

    #[test]
    fn empty_passphrases_are_refused() {
        assert!(matches!(
            Envelope::seal_with(&pw(""), b"x", FAST),
            Err(VaultError::EmptyPassphrase)
        ));
    }

    #[test]
    fn sealer_binds_the_context() {
        let s = Sealer::new(random_key().unwrap());
        let blob = s.seal(b"history.text", b"select 1").unwrap();
        assert_eq!(&*s.open(b"history.text", &blob).unwrap(), b"select 1");
        assert!(s.open(b"saved.text", &blob).is_err());
        let other = Sealer::new(random_key().unwrap());
        assert!(other.open(b"history.text", &blob).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn sealer_round_trips(data in proptest::collection::vec(any::<u8>(), 0..2048), ctx in proptest::collection::vec(any::<u8>(), 0..32)) {
            let s = Sealer::new(random_key().unwrap());
            let blob = s.seal(&ctx, &data).unwrap();
            prop_assert_eq!(&*s.open(&ctx, &blob).unwrap(), &data[..]);
        }

        #[test]
        fn opening_garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = Envelope::open(&pw("p"), &data);
            let s = Sealer::new(random_key().unwrap());
            let _ = s.open(b"", &data);
        }
    }
}
