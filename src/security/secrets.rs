//! AES-256-GCM secret box for encrypting credentials at rest.
//! Ciphertext format: base64(nonce || ct), RawStdEncoding — byte-compatible
//! with the Go implementation.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::engine::general_purpose::STANDARD as B64_PAD;
use base64::Engine;

#[derive(Debug, thiserror::Error)]
pub enum SecretBoxError {
    #[error("APP_SECRET_KEY must be valid base64")]
    InvalidKey,
    #[error("APP_SECRET_KEY must decode to 32 bytes")]
    InvalidKeyLength,
    #[error("invalid ciphertext")]
    InvalidCiphertext,
    #[error("encrypt/decrypt failed")]
    Crypto,
}

pub struct SecretBox {
    key: [u8; 32],
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(***)")
    }
}

impl SecretBox {
    /// Creates a box from a base64 32-byte key. In dev, an empty key string
    /// generates an ephemeral key (callers must then use ONE shared instance).
    pub fn new(secret_key_base64: Option<&str>) -> Result<Self, SecretBoxError> {
        match secret_key_base64 {
            None => {
                use rand::RngCore;
                let mut key = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut key);
                Ok(Self { key })
            }
            Some(s) => {
                let decoded = B64_PAD
                    .decode(s.trim())
                    .map_err(|_| SecretBoxError::InvalidKey)?;
                let key: [u8; 32] = decoded
                    .try_into()
                    .map_err(|_| SecretBoxError::InvalidKeyLength)?;
                Ok(Self { key })
            }
        }
    }

    pub fn encrypt_to_string(&self, plaintext: &[u8]) -> Result<String, SecretBoxError> {
        let cipher = Aes256Gcm::new((&self.key).into());
        let mut nonce_bytes = [0u8; 12];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, Payload { msg: plaintext, aad: &[] })
            .map_err(|_| SecretBoxError::Crypto)?;
        Ok(B64.encode([&nonce_bytes[..], ct.as_slice()].concat()))
    }

    pub fn decrypt_string(&self, enc: &str) -> Result<Vec<u8>, SecretBoxError> {
        let buf = B64.decode(enc.trim()).map_err(|_| SecretBoxError::InvalidCiphertext)?;
        if buf.len() < 12 {
            return Err(SecretBoxError::InvalidCiphertext);
        }
        let (nonce, ct) = buf.split_at(12);
        let cipher = Aes256Gcm::new((&self.key).into());
        cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: &[] })
            .map_err(|_| SecretBoxError::Crypto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_b64() -> String {
        B64_PAD.encode([7u8; 32])
    }

    #[test]
    fn roundtrip() {
        let sb = SecretBox::new(Some(&key_b64())).unwrap();
        let enc = sb.encrypt_to_string(b"hunter2-password").unwrap();
        assert_ne!(enc, "hunter2-password");
        assert_eq!(sb.decrypt_string(&enc).unwrap(), b"hunter2-password");
    }

    #[test]
    fn unique_nonces() {
        let sb = SecretBox::new(Some(&key_b64())).unwrap();
        let e1 = sb.encrypt_to_string(b"same").unwrap();
        let e2 = sb.encrypt_to_string(b"same").unwrap();
        assert_ne!(e1, e2, "nonce reuse must never happen");
    }

    #[test]
    fn wrong_key_fails() {
        let sb1 = SecretBox::new(Some(&B64_PAD.encode([1u8; 32]))).unwrap();
        let sb2 = SecretBox::new(Some(&B64_PAD.encode([2u8; 32]))).unwrap();
        let enc = sb1.encrypt_to_string(b"secret").unwrap();
        assert!(sb2.decrypt_string(&enc).is_err());
    }

    #[test]
    fn invalid_keys_rejected() {
        assert!(matches!(
            SecretBox::new(Some("not-base64!!!")),
            Err(SecretBoxError::InvalidKey)
        ));
        assert!(matches!(
            SecretBox::new(Some(&B64_PAD.encode([1u8; 16]))),
            Err(SecretBoxError::InvalidKeyLength)
        ));
    }
}
