//! The obfuscation other clients use for saved passwords. Both keys below are published in those
//! projects' source code: they protect nothing against someone who has the file, which is why
//! Treasury moves the secrets into the system keychain on import.

use aes::{Aes128, Aes256};
use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// DBeaver's fixed key for `credentials-config.json` (`BaseProjectImpl.LOCAL_KEY_CACHE`).
const DBEAVER_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// Beekeeper Studio's default key, which seals the per-install key in its `.key` file.
pub const BEEKEEPER_DEFAULT_KEY: &str =
    "38782F413F442A472D4B6150645367566B59703373367639792442264529482B";

/// Decrypts DBeaver's `credentials-config.json`: AES-128-CBC, IV in the first 16 bytes.
pub fn dbeaver_credentials(data: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    if data.len() < 32 {
        return None;
    }
    let (iv, body) = data.split_at(16);
    let dec = cbc::Decryptor::<Aes128>::new_from_slices(&DBEAVER_KEY, iv).ok()?;
    dec.decrypt_padded_vec::<Pkcs7>(body)
        .ok()
        .map(Zeroizing::new)
}

/// Decrypts a value written by the `simple-encryptor` npm package with `key` (HMAC-SHA256 hex,
/// then IV hex, then base64 of AES-256-CBC over JSON) and returns the JSON value.
pub fn simple_encryptor(key: &str, text: &str) -> Option<serde_json::Value> {
    let mut crypto_key = Zeroizing::new([0u8; 32]);
    crypto_key.copy_from_slice(&Sha256::digest(key.as_bytes()));
    let text = text.trim();
    let (mac_hex, rest) = (text.get(..64)?, text.get(64..)?);
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(crypto_key.as_slice()).ok()?;
    mac.update(rest.as_bytes());
    let expected = decode_hex(mac_hex)?;
    mac.verify_slice(&expected).ok()?;
    let iv = decode_hex(rest.get(..32)?)?;
    let body = decode_base64(rest.get(32..)?)?;
    let dec = cbc::Decryptor::<Aes256>::new_from_slices(crypto_key.as_slice(), &iv).ok()?;
    let plain = Zeroizing::new(dec.decrypt_padded_vec::<Pkcs7>(&body).ok()?);
    serde_json::from_slice(&plain).ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let trimmed: Vec<u8> = bytes.iter().copied().take_while(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    for chunk in trimmed.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
pub mod testing {
    //! Encryption in the other clients' formats, for building fixtures.
    use super::*;
    use cbc::cipher::BlockModeEncrypt;

    pub fn dbeaver_encrypt(plain: &[u8]) -> Vec<u8> {
        let mut iv = [0u8; 16];
        getrandom::fill(&mut iv).unwrap();
        let enc = cbc::Encryptor::<Aes128>::new_from_slices(&DBEAVER_KEY, &iv).unwrap();
        let mut out = iv.to_vec();
        out.extend(enc.encrypt_padded_vec::<Pkcs7>(plain));
        out
    }

    pub fn simple_encrypt(key: &str, value: &serde_json::Value) -> String {
        let crypto_key = Sha256::digest(key.as_bytes());
        let mut iv = [0u8; 16];
        getrandom::fill(&mut iv).unwrap();
        let enc = cbc::Encryptor::<Aes256>::new_from_slices(crypto_key.as_slice(), &iv).unwrap();
        let body = enc.encrypt_padded_vec::<Pkcs7>(value.to_string().as_bytes());
        let rest = format!(
            "{}{}",
            treasury_core::format::hex(&iv),
            treasury_core::json::base64(&body)
        );
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(crypto_key.as_slice()).unwrap();
        mac.update(rest.as_bytes());
        format!(
            "{}{rest}",
            treasury_core::format::hex(&mac.finalize().into_bytes())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn dbeaver_round_trip() {
        let sealed = dbeaver_encrypt(b"{\"a\":1}");
        assert_eq!(&*dbeaver_credentials(&sealed).unwrap(), b"{\"a\":1}");
        assert!(dbeaver_credentials(b"short").is_none());
    }

    #[test]
    fn simple_encryptor_round_trip_and_hmac() {
        let v = serde_json::json!({"encryptionKey": "abc"});
        let s = simple_encrypt(BEEKEEPER_DEFAULT_KEY, &v);
        assert_eq!(simple_encryptor(BEEKEEPER_DEFAULT_KEY, &s), Some(v));
        let mut tampered = s.clone();
        tampered.replace_range(70..71, if &s[70..71] == "a" { "b" } else { "a" });
        assert!(simple_encryptor(BEEKEEPER_DEFAULT_KEY, &tampered).is_none());
        assert!(simple_encryptor("another key at least 16", &s).is_none());
    }

    #[test]
    fn base64_matches_the_encoder() {
        for data in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foobar",
            &[0, 255, 128, 7, 9],
        ] {
            assert_eq!(
                decode_base64(&treasury_core::json::base64(data)).unwrap(),
                data
            );
        }
    }
}
