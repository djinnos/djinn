/// AES-256-GCM encryption/decryption for the credential vault.
///
/// The encryption key is stored as 32 raw bytes in a file on disk. The path
/// is resolved by [`vault_key_path`]:
///   1. `DJINN_VAULT_KEY_PATH` if set (tests, future KMS integrations, etc.)
///   2. `${DJINN_HOME}/vault.key` if `DJINN_HOME` is set
///   3. `${HOME}/.djinn/vault.key` as the final fallback
///
/// On first use, if the file does not exist, 32 random bytes are generated via
/// `ring::rand::SystemRandom`, written atomically (tmp + rename), and
/// permissions set to `0600` on Unix. Subsequent calls read the file directly.
///
/// Historically the key was derived from `SHA-256("djinn-credential-key:{hostname}:{user}")`
/// which broke any time the container hostname changed (recreating the vault
/// data as undecryptable). The file-backed key survives restarts and recreations
/// as long as the data volume is persistent.
///
/// Stored ciphertext format: nonce (12 bytes) || AES-256-GCM ciphertext+tag.
use std::path::PathBuf;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{Error, Result};

const KEY_LEN: usize = 32;

/// Resolve the on-disk path for the vault key.
fn vault_key_path() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("DJINN_VAULT_KEY_PATH") {
        if !explicit.is_empty() {
            return Ok(PathBuf::from(explicit));
        }
    }
    if let Ok(djinn_home) = std::env::var("DJINN_HOME") {
        if !djinn_home.is_empty() {
            return Ok(PathBuf::from(djinn_home).join("vault.key"));
        }
    }
    let home = dirs::home_dir()
        .ok_or_else(|| Error::InvalidData("cannot determine home directory".into()))?;
    Ok(home.join(".djinn").join("vault.key"))
}

/// Load the 32-byte key from disk, generating it on first use.
fn load_or_create_key_bytes() -> Result<[u8; KEY_LEN]> {
    let path = vault_key_path()?;

    match std::fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() != KEY_LEN {
                return Err(Error::InvalidData(format!(
                    "vault key file {} has invalid length {} (expected {})",
                    path.display(),
                    bytes.len(),
                    KEY_LEN
                )));
            }
            let mut out = [0u8; KEY_LEN];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => generate_and_persist(&path),
        Err(e) => Err(Error::InvalidData(format!(
            "failed to read vault key {}: {e}",
            path.display()
        ))),
    }
}

/// Generate a fresh 32-byte key, persist it atomically, and return it.
fn generate_and_persist(path: &std::path::Path) -> Result<[u8; KEY_LEN]> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::InvalidData(format!(
                "failed to create vault key parent {}: {e}",
                parent.display()
            ))
        })?;
    }

    let rng = SystemRandom::new();
    let mut key = [0u8; KEY_LEN];
    rng.fill(&mut key)
        .map_err(|_| Error::InvalidData("failed to generate vault key material".into()))?;

    // Write to a sibling `.tmp` file, then rename into place so a crash
    // mid-write cannot leave a truncated key file that permanently locks
    // out the vault.
    let tmp_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".tmp");
        PathBuf::from(p)
    };

    write_key_file(&tmp_path, &key)?;

    std::fs::rename(&tmp_path, path).map_err(|e| {
        // Best-effort cleanup; ignore secondary errors.
        let _ = std::fs::remove_file(&tmp_path);
        Error::InvalidData(format!(
            "failed to rename vault key into place at {}: {e}",
            path.display()
        ))
    })?;

    Ok(key)
}

#[cfg(unix)]
fn write_key_file(path: &std::path::Path, key: &[u8; KEY_LEN]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            Error::InvalidData(format!(
                "failed to open vault key tmp {}: {e}",
                path.display()
            ))
        })?;
    f.write_all(key).map_err(|e| {
        Error::InvalidData(format!(
            "failed to write vault key tmp {}: {e}",
            path.display()
        ))
    })?;
    f.sync_all().map_err(|e| {
        Error::InvalidData(format!(
            "failed to fsync vault key tmp {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn write_key_file(path: &std::path::Path, key: &[u8; KEY_LEN]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|e| {
            Error::InvalidData(format!(
                "failed to open vault key tmp {}: {e}",
                path.display()
            ))
        })?;
    f.write_all(key).map_err(|e| {
        Error::InvalidData(format!(
            "failed to write vault key tmp {}: {e}",
            path.display()
        ))
    })?;
    f.sync_all().map_err(|e| {
        Error::InvalidData(format!(
            "failed to fsync vault key tmp {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Build an AES-256-GCM key from the on-disk vault key file.
///
/// The file is read on every call. This is intentionally uncached — encrypt
/// and decrypt are not hot paths (they run only when credentials are stored
/// or loaded), the file is 32 bytes, and keeping things stateless makes tests
/// that manipulate `DJINN_VAULT_KEY_PATH` trivial.
fn build_key() -> Result<LessSafeKey> {
    let key_bytes = load_or_create_key_bytes()?;
    let unbound = UnboundKey::new(&AES_256_GCM, &key_bytes)
        .map_err(|_| Error::InvalidData("failed to construct AES-256 key".into()))?;
    Ok(LessSafeKey::new(unbound))
}

/// Encrypt `plaintext` and return `nonce || ciphertext+tag` as a byte vec.
pub fn encrypt(plaintext: &str) -> Result<Vec<u8>> {
    let key = build_key()?;
    let rng = SystemRandom::new();

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| Error::InvalidData("failed to generate nonce".into()))?;

    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| Error::InvalidData("encryption failed".into()))?;

    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&in_out);
    Ok(blob)
}

/// Decrypt a blob produced by [`encrypt`].
pub fn decrypt(blob: &[u8]) -> Result<String> {
    if blob.len() < NONCE_LEN {
        return Err(Error::InvalidData(
            "invalid encrypted blob: too short".into(),
        ));
    }

    let key = build_key()?;
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);

    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes)
        .map_err(|_| Error::InvalidData("invalid nonce length".into()))?;

    let mut in_out = ciphertext.to_vec();
    let plaintext = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| Error::InvalidData("decryption failed — wrong key or corrupt data".into()))?;

    String::from_utf8(plaintext.to_vec())
        .map_err(|_| Error::InvalidData("decrypted value is not valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests mutate `DJINN_VAULT_KEY_PATH`, which is process-global. Serialise
    // them so they don't race each other. Other env (`DJINN_HOME`, `HOME`)
    // doesn't matter here because `DJINN_VAULT_KEY_PATH` takes precedence.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct KeyPathGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tempdir: tempfile::TempDir,
        prev: Option<String>,
    }

    impl KeyPathGuard {
        fn new() -> (Self, PathBuf) {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let tempdir = tempfile::tempdir().expect("tempdir");
            let key_path = tempdir.path().join("vault.key");
            let prev = std::env::var("DJINN_VAULT_KEY_PATH").ok();
            unsafe {
                std::env::set_var("DJINN_VAULT_KEY_PATH", &key_path);
            }
            (
                Self {
                    _lock: lock,
                    _tempdir: tempdir,
                    prev,
                },
                key_path,
            )
        }
    }

    impl Drop for KeyPathGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DJINN_VAULT_KEY_PATH", v),
                    None => std::env::remove_var("DJINN_VAULT_KEY_PATH"),
                }
            }
        }
    }

    #[test]
    fn round_trip() {
        let (_guard, _path) = KeyPathGuard::new();
        let plaintext = "sk-ant-api03-test-key";
        let blob = encrypt(plaintext).unwrap();
        let recovered = decrypt(&blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertexts() {
        let (_guard, _path) = KeyPathGuard::new();
        let blob1 = encrypt("same-value").unwrap();
        let blob2 = encrypt("same-value").unwrap();
        // Nonces are random — blobs must differ even for identical plaintext.
        assert_ne!(blob1, blob2);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let (_guard, _path) = KeyPathGuard::new();
        let mut blob = encrypt("secret").unwrap();
        // Flip a byte in the ciphertext portion.
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decrypt(&blob).is_err());
    }

    #[test]
    fn too_short_blob_fails() {
        let (_guard, _path) = KeyPathGuard::new();
        assert!(decrypt(&[0u8; 5]).is_err());
    }

    #[test]
    fn missing_file_generates_fresh_key() {
        let (_guard, path) = KeyPathGuard::new();
        assert!(!path.exists(), "precondition: key file must not exist yet");

        // Trigger generation.
        let _blob = encrypt("hello").unwrap();

        assert!(path.exists(), "encrypt should have created the key file");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), KEY_LEN, "key file must be exactly 32 bytes");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "key file permissions must be 0600");
        }
    }

    #[test]
    fn existing_file_is_reused() {
        let (_guard, path) = KeyPathGuard::new();

        // Seed a known 32-byte key.
        let known = [0x42u8; KEY_LEN];
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, known).unwrap();
        let before_meta = std::fs::metadata(&path).unwrap();
        let before_len = before_meta.len();
        let before_mtime = before_meta.modified().ok();

        // Round trip with the seeded key.
        let blob = encrypt("reuse me").unwrap();
        let recovered = decrypt(&blob).unwrap();
        assert_eq!(recovered, "reuse me");

        // File must not have been rewritten.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, known, "key file contents must be unchanged");
        let after_meta = std::fs::metadata(&path).unwrap();
        assert_eq!(after_meta.len(), before_len);
        if let (Some(a), Some(b)) = (before_mtime, after_meta.modified().ok()) {
            assert_eq!(a, b, "mtime should be unchanged — file was not rewritten");
        }
    }

    #[test]
    fn wrong_sized_file_errors() {
        let (_guard, path) = KeyPathGuard::new();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"too-short!").unwrap(); // 10 bytes

        let err = encrypt("nope").expect_err("encrypt must fail on wrong-sized key file");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("invalid length"),
            "error should mention invalid length, got: {msg}"
        );
    }
}
