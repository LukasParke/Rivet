//! RFC 6238 TOTP (and the underlying RFC 4226 HOTP) implemented on SHA-1/HMAC.
//!
//! Secrets travel as unpadded RFC 4648 base32, matching authenticator-app
//! conventions (otpauth URIs, manual entry codes).

use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// 20 random bytes as unpadded base32 (160-bit secret, per RFC 4226 §4).
pub fn generate_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    base32_encode(&bytes)
}

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([0, buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0]) >> 16;
        let n = (chunk.len() * 8).div_ceil(5);
        for k in 0..n {
            out.push(ALPHABET[(bits >> (35 - 5 * k)) as usize & 31] as char);
        }
    }
    out
}
/// Strict-ish base32 decode: A–Z/2–7 only, whitespace tolerated, no padding.
pub fn base32_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            '=' => continue,
            _ => return Err(format!("invalid base32 character {c:?}")),
        };
        acc = (acc << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

fn hotp(key: &[u8], counter: u64, digits: u32) -> String {
    let mut mac = HmacSha1::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let off = (digest[19] & 0x0f) as usize;
    let code = (u32::from_be_bytes([
        digest[off],
        digest[off + 1],
        digest[off + 2],
        digest[off + 3],
    ]) & 0x7fff_ffff) as u64;
    let modulus = 10u64.pow(digits);
    format!("{:0width$}", code % modulus, width = digits as usize)
}

/// TOTP for an explicit instant (`now` seconds since `t0`, in `step` buckets).
pub fn totp_at(
    secret_b32: &str,
    digits: u32,
    step: u64,
    t0: u64,
    now: u64,
) -> Result<String, String> {
    if step == 0 || !(1..=10).contains(&digits) {
        return Err("invalid totp parameters".into());
    }
    let key = base32_decode(secret_b32)?;
    if key.is_empty() {
        return Err("empty totp secret".into());
    }
    let counter = now.saturating_sub(t0) / step;
    Ok(hotp(&key, counter, digits))
}

/// Current-time convenience wrapper (T0=0, step=30 defaults live at callers).
pub fn totp_now(secret_b32: &str, digits: u32, step: u64, t0: u64) -> Result<String, String> {
    totp_at(
        secret_b32,
        digits,
        step,
        t0,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock before unix epoch".to_string())?
            .as_secs(),
    )
}

/// Verifies a code of exactly `digits` digits against the current or
/// ±`window` adjacent steps (RFC 6238 §5.2 recommends accepting drift).
/// Codes whose length differs from the expected digit count are rejected
/// before any comparison. Returns the matched timestep on success so callers
/// can enforce single use; comparison is constant-shape: both sides are
/// rendered digit strings of equal length before comparing.
pub fn verify(
    secret_b32: &str,
    code: &str,
    digits: u32,
    window: u64,
) -> Result<Option<u64>, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock before unix epoch".to_string())?
        .as_secs();
    verify_at(secret_b32, code, digits, window, now)
}

/// Drift-window verification against a fixed instant; testable core of [`verify`].
pub fn verify_at(
    secret_b32: &str,
    code: &str,
    digits: u32,
    window: u64,
    now: u64,
) -> Result<Option<u64>, String> {
    const STEP: u64 = 30;
    let code = code.trim();
    let len = code.chars().count();
    if len != digits as usize || code.chars().any(|c| !c.is_ascii_digit()) {
        // Shape rejection happens without leaking timing beyond length.
        return Ok(None);
    }
    let candidate = totp_at(secret_b32, digits, STEP, 0, now)?;
    if subtle_eq(&candidate, code) {
        return Ok(Some(now / STEP));
    }
    for drift in 1..=window {
        let Some(past) = now.checked_sub(drift * STEP) else {
            break;
        };
        if subtle_eq(&totp_at(secret_b32, digits, STEP, 0, past)?, code) {
            return Ok(Some(past / STEP));
        }
        let future = now.saturating_add(drift * STEP);
        if subtle_eq(&totp_at(secret_b32, digits, STEP, 0, future)?, code) {
            return Ok(Some(future / STEP));
        }
    }
    Ok(None)
}

/// Equal-length string comparison (both inputs already validated shapes).
fn subtle_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// otpauth URI for authenticator provisioning (QR-encodable).
pub fn otpauth_uri(label_email: &str, secret_b32: &str) -> String {
    let label = label_email.replace(':', "");
    format!("otpauth://totp/Rivet:{label}?secret={secret_b32}&issuer=Rivet&digits=6&period=30")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard RFC 6238 Appendix B secret ("12345678901234567890") in base32.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn rfc6238_sha1_vectors() {
        let cases: &[(u64, &str)] = &[
            (59, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for &(t, expected) in cases {
            assert_eq!(
                totp_at(RFC_SECRET, 8, 30, 0, t).unwrap(),
                expected,
                "vector at T={t}"
            );
        }
    }

    #[test]
    fn rfc4226_hotp_vectors_via_totp_at() {
        // HOTP vectors expressed as TOTP with one giant step (T=counter*30).
        let hotp_cases: &[(u64, &str)] = &[(0, "755224"), (1, "287082"), (9, "520489")];
        for &(counter, expected) in hotp_cases {
            // Use raw counter via step trick: counter = t/30 with step 30 →
            // instead call hotp directly through totp math by scaling time.
            let t = counter * 30;
            assert_eq!(totp_at(RFC_SECRET, 6, 30, 0, t).unwrap(), expected);
        }
    }

    #[test]
    fn drift_window_accepts_adjacent_steps_and_rejects_far_ones() {
        let secret = generate_secret();
        let now = 1_700_000_000u64;
        let cur = totp_at(&secret, 6, 30, 0, now).unwrap();
        let prev = totp_at(&secret, 6, 30, 0, now - 30).unwrap();
        let next = totp_at(&secret, 6, 30, 0, now + 30).unwrap();
        let far = totp_at(&secret, 6, 30, 0, now + 120).unwrap();

        assert!(verify_at(&secret, &cur, 6, 1, now).unwrap().is_some());
        assert!(verify_at(&secret, &prev, 6, 1, now).unwrap().is_some());
        assert!(verify_at(&secret, &next, 6, 1, now).unwrap().is_some());
        assert_eq!(verify_at(&secret, &far, 6, 1, now).unwrap(), None);
        assert!(verify_at(&secret, &far, 6, 4, now).unwrap().is_some());
    }

    #[test]
    fn bad_base32_rejected() {
        assert!(base32_decode("abc1!").is_err());
        assert!(totp_now("not!!valid", 6, 30, 0).is_err());
        assert!(
            verify_at("", "123456", 6, 1, 100).is_err(),
            "empty secret errors"
        );
    }

    #[test]
    fn non_numeric_code_fails_cleanly() {
        let secret = generate_secret();
        assert_eq!(verify_at(&secret, "abcdef", 6, 1, 1_700_000_000).unwrap(), None);
        assert_eq!(verify_at(&secret, "", 6, 1, 1_700_000_000).unwrap(), None);
    }

    #[test]
    fn base32_roundtrip() {
        for len in [0usize, 1, 4, 5, 6, 20] {
            let data: Vec<u8> = (0..len as u8).collect();
            let enc = base32_encode(&data);
            assert!(!enc.contains('='), "unpadded");
            assert_eq!(base32_decode(&enc).unwrap(), data, "roundtrip len={len}");
        }
        assert_eq!(base32_decode("gezd gnbv").unwrap(), b"12345".to_vec());
        assert_eq!(
            base32_decode("GEZDGNBV").unwrap(),
            base32_decode("gezdgnbv").unwrap()
        );
    }

    #[test]
    fn generated_secrets_are_well_formed() {
        for _ in 0..8 {
            let s = generate_secret();
            assert_eq!(s.len(), 32, "20 bytes → exactly 32 unpadded chars");
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
            assert_eq!(base32_decode(&s).unwrap().len(), 20);
        }
    }

    #[test]
    fn otpauth_uri_shape() {
        let uri = otpauth_uri("user@example.com", "JBSWY3DPEHPK3PXP");
        assert_eq!(
            uri,
            "otpauth://totp/Rivet:user@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Rivet&digits=6&period=30"
        );
        assert_eq!(
            otpauth_uri("we:ird", "ABC"),
            "otpauth://totp/Rivet:weird?secret=ABC&issuer=Rivet&digits=6&period=30"
        );
    }

    #[test]
    fn invalid_parameters_rejected() {
        assert!(totp_at(RFC_SECRET, 6, 0, 0, 59).is_err(), "step 0 invalid");
        assert!(
            totp_at(RFC_SECRET, 0, 30, 0, 59).is_err(),
            "digits 0 invalid"
        );
        assert!(
            totp_at(RFC_SECRET, 11, 30, 0, 59).is_err(),
            "digits >10 invalid"
        );
    }

    #[test]
    fn code_length_must_match_expected_digits() {
        let secret = generate_secret();
        let now = 1_700_000_000u64;
        let cur = totp_at(&secret, 6, 30, 0, now).unwrap();
        assert_eq!(cur.chars().count(), 6);

        // 5- and 7-digit mutations of a live code are rejected when digits=6.
        assert_eq!(verify_at(&secret, &cur[..5], 6, 1, now).unwrap(), None);
        assert_eq!(verify_at(&secret, &format!("0{cur}"), 6, 1, now).unwrap(), None);
        // An 8-digit string is rejected too — even a genuinely live 8-digit
        // TOTP — because it does not match the expected digit count...
        let eight = totp_at(&secret, 8, 30, 0, now).unwrap();
        assert_eq!(verify_at(&secret, &eight, 6, 1, now).unwrap(), None);
        // ...but is accepted once the caller expects 8 digits.
        assert!(verify_at(&secret, &eight, 8, 1, now).unwrap().is_some());
    }

    #[test]
    fn verify_returns_matched_timestep() {
        let secret = generate_secret();
        let now = 1_700_000_000u64;
        let prev = totp_at(&secret, 6, 30, 0, now - 30).unwrap();
        let next = totp_at(&secret, 6, 30, 0, now + 30).unwrap();
        assert_eq!(
            verify_at(&secret, &prev, 6, 1, now).unwrap(),
            Some((now - 30) / 30)
        );
        assert_eq!(
            verify_at(&secret, &next, 6, 1, now).unwrap(),
            Some((now + 30) / 30)
        );
    }
}
