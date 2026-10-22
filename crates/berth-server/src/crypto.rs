//! Encryption of secrets stored in the database (storage keys, later OAuth tokens).
//!
//! AES-256-GCM under a key derived from `secret.key`, a fresh 96-bit nonce per record, and
//! the owning record's id as associated data, so a ciphertext cannot be moved to another row.
//! Stored form: base64 of `nonce || ciphertext || tag`.

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::SecretKey;

/// Stored secrets are encrypted with a key derived from `secret.key`, separate from other uses.
const SECRETS_KEY_LABEL: &[u8] = b"berth stored secrets v1";
const NONCE_LEN: usize = 12;

fn cipher(secret: &SecretKey) -> Aes256Gcm {
    Aes256Gcm::new(&secret.hmac(&[SECRETS_KEY_LABEL]).into())
}

/// Encrypts `plain` for the record `record_id`.
pub(crate) fn encrypt(secret: &SecretKey, record_id: &str, plain: &[u8]) -> anyhow::Result<String> {
    let mut nonce = [0; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    let sealed = cipher(secret)
        .encrypt(
            &nonce.into(),
            Payload {
                msg: plain,
                aad: record_id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(STANDARD.encode(out))
}

/// Decrypts a value written by [`encrypt`] for the same record.
pub(crate) fn decrypt(
    secret: &SecretKey,
    record_id: &str,
    stored: &str,
) -> anyhow::Result<Vec<u8>> {
    let raw = STANDARD
        .decode(stored)
        .context("stored secret is not base64")?;
    anyhow::ensure!(raw.len() > NONCE_LEN, "stored secret is truncated");
    let (nonce, sealed) = raw.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
    cipher(secret)
        .decrypt(
            &nonce.into(),
            Payload {
                msg: sealed,
                aad: record_id.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("stored secret cannot be decrypted: wrong secret.key or tampered data")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_fresh_nonces() {
        let secret = SecretKey::from_bytes([7; 32]);
        let a = encrypt(&secret, "p1", b"keys").unwrap();
        let b = encrypt(&secret, "p1", b"keys").unwrap();
        assert_ne!(a, b, "a nonce per record");
        assert_eq!(decrypt(&secret, "p1", &a).unwrap(), b"keys");
        assert!(!a.contains("keys"));
    }

    #[test]
    fn rejects_other_records_keys_and_tampering() {
        let secret = SecretKey::from_bytes([7; 32]);
        let stored = encrypt(&secret, "p1", b"keys").unwrap();
        assert!(
            decrypt(&secret, "p2", &stored).is_err(),
            "bound to its record"
        );
        assert!(decrypt(&SecretKey::from_bytes([8; 32]), "p1", &stored).is_err());
        let mut raw = STANDARD.decode(&stored).unwrap();
        raw[NONCE_LEN] ^= 1;
        assert!(decrypt(&secret, "p1", &STANDARD.encode(raw)).is_err());
        assert!(decrypt(&secret, "p1", "AAAA").is_err());
    }
}
