//! Argon2id password hashing, compatible with the original encoded format
//! (`argon2id$v=19$m=<m>,t=<t>,p=<p>$<salt_b64>$<hash_b64>`, RawStdEncoding)
//! and with standard PHC strings (`$argon2id$v=19$...`).

use argon2::Argon2;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;

const MEMORY_KIB: u32 = 64 * 1024; // 64 MiB
const TIME_COST: u32 = 3;
const THREADS: u32 = 2;
const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("password must be at least 12 characters")]
    TooShort,
    #[error("invalid argon2 encoding")]
    InvalidEncoding,
}

/// Hashes `password` with Argon2id (m=64MiB, t=3, p=2, 32-byte key).
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    if password.len() < 12 {
        return Err(PasswordError::TooShort);
    }
    let mut salt = [0u8; SALT_LEN];
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(&mut salt);

    let params = argon2::Params::new(MEMORY_KIB, TIME_COST, THREADS, Some(KEY_LEN))
        .map_err(|_| PasswordError::InvalidEncoding)?;
    let hasher = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    hasher
        .hash_password_into(password.as_bytes(), &salt, &mut out)
        .map_err(|_| PasswordError::InvalidEncoding)?;

    Ok(format!(
        "argon2id$v=19$m={MEMORY_KIB},t={TIME_COST},p={THREADS}${}${}",
        B64.encode(salt),
        B64.encode(out)
    ))
}

/// Verifies `password` against an encoded hash in either supported format.
/// Runs in blocking context from async callers (argon2 is CPU-heavy).
pub fn verify_password(password: &str, encoded: &str) -> Result<bool, PasswordError> {
    let (salt, hash, params) = parse_encoded(encoded)?;
    let hasher = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = vec![0u8; hash.len()];
    hasher
        .hash_password_into(password.as_bytes(), &salt, &mut out)
        .map_err(|_| PasswordError::InvalidEncoding)?;
    Ok(constant_time_eq(&out, &hash))
}

type Parsed = (Vec<u8>, Vec<u8>, argon2::Params);

fn parse_encoded(encoded: &str) -> Result<Parsed, PasswordError> {
    // Strip optional leading '$' and optional "argon2id" prefix so both
    // "argon2id$v=19$..." and "$argon2id$v=19$..." parse.
    let s = encoded.trim_start_matches('$');
    let s = s.strip_prefix("argon2id$").unwrap_or(s);

    let mut parts = s.split('$');
    let header = parts.next().ok_or(PasswordError::InvalidEncoding)?;
    if !header.starts_with("v=19") {
        return Err(PasswordError::InvalidEncoding);
    }
    let params_str = parts.next().ok_or(PasswordError::InvalidEncoding)?;
    let salt_b64 = parts.next().ok_or(PasswordError::InvalidEncoding)?;
    let hash_b64 = parts.next().ok_or(PasswordError::InvalidEncoding)?;

    let mut memory = MEMORY_KIB;
    let mut time_cost = TIME_COST;
    let mut threads = THREADS;
    for kv in params_str.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            return Err(PasswordError::InvalidEncoding);
        };
        match k {
            "m" => memory = v.parse().map_err(|_| PasswordError::InvalidEncoding)?,
            "t" => time_cost = v.parse().map_err(|_| PasswordError::InvalidEncoding)?,
            "p" => threads = v.parse().map_err(|_| PasswordError::InvalidEncoding)?,
            _ => return Err(PasswordError::InvalidEncoding),
        }
    }

    let salt = B64.decode(salt_b64).map_err(|_| PasswordError::InvalidEncoding)?;
    let hash = B64.decode(hash_b64).map_err(|_| PasswordError::InvalidEncoding)?;
    if salt.len() < 8 || hash.is_empty() {
        return Err(PasswordError::InvalidEncoding);
    }
    let params = argon2::Params::new(memory, time_cost, threads, Some(hash.len()))
        .map_err(|_| PasswordError::InvalidEncoding)?;
    Ok((salt, hash, params))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_rejects_short() {
        let h = hash_password("correct-horse-battery").unwrap();
        assert!(h.starts_with("argon2id$v=19$m=65536,t=3,p=2$"));
        assert!(verify_password("correct-horse-battery", &h).unwrap());
        assert!(!verify_password("wrong-password-xx", &h).unwrap());
        assert!(matches!(
            hash_password("short"),
            Err(PasswordError::TooShort)
        ));
    }

    #[test]
    fn verifies_phc_format_too() {
        // Standard PHC string with leading $ — produced by the argon2 crate.
        // Build a PHC-format string (leading $) from our own encoding.
        let h = hash_password("correct-horse-battery").unwrap();
        let rest = h.strip_prefix("argon2id$").expect("prefix");
        let phc = format!("${rest}");
        assert!(verify_password("correct-horse-battery", &phc).unwrap());
    }

    #[test]
    fn wrong_params_rejected() {
        assert!(verify_password("x", "garbage").is_err());
    }
}
