/// AES-256-GCM encryption/decryption for the credential vault.
///
/// The encryption key is derived from machine identity (hostname + username)
/// via SHA-256, providing machine-binding without a user-managed master password.
///
/// Stored format: nonce (12 bytes) || AES-256-GCM ciphertext+tag.
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{Error, Result};

fn machine_key_material() -> String {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    let hostname = if hostname.is_empty() {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string())
    } else {
        hostname
    };
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("djinn-credential-key:{hostname}:{user}")
}

fn build_key() -> LessSafeKey {
    let material = machine_key_material();
    let digest = ring::digest::digest(&ring::digest::SHA256, material.as_bytes());
    // SHA-256 output is always 32 bytes — exactly what AES-256-GCM requires.
    let unbound = UnboundKey::new(&AES_256_GCM, digest.as_ref())
        .expect("SHA-256 always produces a valid AES-256 key");
    LessSafeKey::new(unbound)
}

/// Encrypt `plaintext` and return `nonce || ciphertext+tag` as a byte vec.
pub fn encrypt(plaintext: &str) -> Result<Vec<u8>> {
    let key = build_key();
    let rng = SystemRandom::new();

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| Error::Internal("failed to generate nonce".into()))?;

    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| Error::Internal("encryption failed".into()))?;

    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&in_out);
    Ok(blob)
}

/// Decrypt a blob produced by [`encrypt`].
pub fn decrypt(blob: &[u8]) -> Result<String> {
    if blob.len() < NONCE_LEN {
        return Err(Error::Internal("invalid encrypted blob: too short".into()));
    }

    let key = build_key();
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);

    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes)
        .map_err(|_| Error::Internal("invalid nonce length".into()))?;

    let mut in_out = ciphertext.to_vec();
    let plaintext = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| Error::Internal("decryption failed — wrong key or corrupt data".into()))?;

    String::from_utf8(plaintext.to_vec())
        .map_err(|_| Error::Internal("decrypted value is not valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let plaintext = "sk-ant-api03-test-key";
        let blob = encrypt(plaintext).unwrap();
        let recovered = decrypt(&blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertexts() {
        let blob1 = encrypt("same-value").unwrap();
        let blob2 = encrypt("same-value").unwrap();
        // Nonces are random — blobs must differ even for identical plaintext.
        assert_ne!(blob1, blob2);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut blob = encrypt("secret").unwrap();
        // Flip a byte in the ciphertext portion.
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decrypt(&blob).is_err());
    }

    #[test]
    fn too_short_blob_fails() {
        assert!(decrypt(&[0u8; 5]).is_err());
    }
}
