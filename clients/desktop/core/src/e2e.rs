//! End-to-end encryption, byte-compatible with `copyctl`'s `crypto.go` and the
//! Android `E2eCrypto.kt`: Argon2id KDF (salt = sha256("copysync-e2e|"+serverId))
//! and AES-256-GCM with a prepended 12-byte nonce (`nonce || ciphertext+tag`).

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use sha2::{Digest, Sha256};

/// AEAD label carried in `EncMeta.alg`.
pub const ALG: &str = "aes-256-gcm";

/// Derive the 32-byte group key from a passphrase + server id.
pub fn derive_key(pass: &str, server_id: &str) -> [u8; 32] {
    let salt = Sha256::digest(format!("copysync-e2e|{server_id}").as_bytes());
    let argon = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(64 * 1024, 1, 4, Some(32)).expect("argon2 params"),
    );
    let mut out = [0u8; 32];
    argon
        .hash_password_into(pass.as_bytes(), salt.as_slice(), &mut out)
        .expect("argon2 derive");
    out
}

/// Short, non-secret fingerprint so receivers can detect a passphrase mismatch.
pub fn key_id(key: &[u8]) -> String {
    let s = Sha256::digest(key);
    hex::encode(s)[..16].to_string()
}

/// Encrypt: returns `nonce(12) || AES-256-GCM(plaintext)+tag(16)`.
pub fn seal(key: &[u8], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a `nonce || ciphertext+tag` blob.
pub fn open(key: &[u8], raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    if raw.len() < 12 {
        anyhow::bail!("ciphertext too short");
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(&raw[..12]);
    cipher
        .decrypt(nonce, &raw[12..])
        .map_err(|e| anyhow::anyhow!("aes-gcm decrypt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let k = derive_key("hunter2", "srv_abc");
        let ct = seal(&k, b"hello world").unwrap();
        assert_ne!(&ct[12..], b"hello world"); // actually encrypted
        assert_eq!(open(&k, &ct).unwrap(), b"hello world");
    }

    #[test]
    fn wrong_key_fails() {
        let k1 = derive_key("pass1", "srv");
        let k2 = derive_key("pass2", "srv");
        let ct = seal(&k1, b"secret").unwrap();
        assert!(open(&k2, &ct).is_err());
    }

    #[test]
    fn key_id_is_16_hex() {
        let k = derive_key("p", "s");
        let id = key_id(&k);
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // Cross-implementation anchor: for fixed inputs the derived key id must equal
    // what the Go/Kotlin clients compute. The expected value is asserted by the
    // shell interop check (Go `copyctl`/snippet vs `interop e2ekey`).
    #[test]
    fn deterministic() {
        assert_eq!(key_id(&derive_key("p", "s")), key_id(&derive_key("p", "s")));
    }
}
