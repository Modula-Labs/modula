//! TOTP (RFC 6238: SHA-1, 6 digits, 30 s) with replay protection and
//! constant-time comparison, plus single-use backup codes.

use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use totp_rs::{Algorithm, Builder, Totp};
use zeroize::Zeroizing;

use crate::{Error, Result, ct_eq, random_bytes};

pub const STEP_SECS: u64 = 30;
pub const DIGITS: usize = 6;
/// Accept one step either side for clock drift.
pub const SKEW_STEPS: u64 = 1;
const SECRET_LEN: usize = 20;

pub fn generate_secret() -> Result<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(random_bytes::<SECRET_LEN>()?.to_vec()))
}

fn totp(secret: &[u8]) -> Result<Totp> {
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(DIGITS as u8)
        .with_step_duration(STEP_SECS)
        .with_skew(0)
        .with_secret(secret.to_vec())
        .build()
        .map_err(|_| Error::InvalidKey)
}

/// Base32 form for manual entry in authenticator apps.
pub fn secret_base32(secret: &[u8]) -> Zeroizing<String> {
    Zeroizing::new(BASE32_NOPAD.encode(secret))
}

/// `otpauth://` URL for QR codes. Issuer and account are percent-encoded.
pub fn otpauth_url(secret: &[u8], issuer: &str, account: &str) -> Zeroizing<String> {
    let enc = |s: &str| url_encode(s);
    Zeroizing::new(format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        enc(issuer),
        enc(account),
        BASE32_NOPAD.encode(secret),
        enc(issuer),
    ))
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn code_at(secret: &[u8], unix_secs: u64) -> Result<String> {
    Ok(totp(secret)?.generate(unix_secs).to_string())
}

/// Strip spaces/dashes; must then be exactly six ASCII digits.
pub fn normalize_code(input: &str) -> Option<String> {
    let c: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (c.len() == DIGITS && c.bytes().all(|b| b.is_ascii_digit())).then_some(c)
}

/// Returns the matched time step when `code` is valid within ±1 step AND newer
/// than `last_used_step`, so each code (and any older one) works only once.
/// The caller must persist the returned step atomically (compare-and-set).
pub fn verify(
    secret: &[u8],
    code: &str,
    unix_secs: u64,
    last_used_step: Option<u64>,
) -> Result<Option<u64>> {
    let Some(code) = normalize_code(code) else {
        return Ok(None);
    };
    let t = totp(secret)?;
    let now_step = unix_secs / STEP_SECS;
    let mut matched = None;
    // Check every candidate (no early exit) so timing doesn't reveal which step matched.
    for step in now_step.saturating_sub(SKEW_STEPS)..=now_step + SKEW_STEPS {
        let candidate = t.generate(step * STEP_SECS).to_string();
        if ct_eq(candidate.as_bytes(), code.as_bytes()) {
            matched = Some(step);
        }
    }
    Ok(matched.filter(|s| last_used_step.is_none_or(|last| *s > last)))
}

pub mod backup {
    //! Backup codes: 10 chars of Crockford base32 (50 bits), shown as `xxxxx-xxxxx`.
    //! Stored as HMAC-SHA256 under a server-held pepper, so a database dump alone
    //! cannot be brute-forced offline.

    use super::*;

    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    pub const COUNT: usize = 10;

    pub fn generate() -> Result<Vec<Zeroizing<String>>> {
        (0..COUNT)
            .map(|_| {
                let raw = random_bytes::<10>()?;
                let chars: String = raw
                    .iter()
                    .map(|b| ALPHABET[(b & 31) as usize] as char)
                    .collect();
                Ok(Zeroizing::new(format!("{}-{}", &chars[..5], &chars[5..])))
            })
            .collect()
    }

    /// Case-insensitive; ignores separators; maps easily-confused letters.
    pub fn normalize(input: &str) -> Option<String> {
        let mut out = String::with_capacity(10);
        for c in input.chars() {
            let c = c.to_ascii_lowercase();
            let c = match c {
                '-' | ' ' => continue,
                'o' => '0',
                'i' | 'l' => '1',
                c => c,
            };
            if !ALPHABET.contains(&(c as u8)) || !c.is_ascii() {
                return None;
            }
            out.push(c);
        }
        (out.len() == 10).then_some(out)
    }

    pub fn hash(pepper: &[u8; 32], normalized: &str) -> [u8; 32] {
        let mut m =
            <Hmac<Sha256> as KeyInit>::new_from_slice(pepper).expect("HMAC accepts any key length");
        m.update(b"modula/backup-code/v1\0");
        m.update(normalized.as_bytes());
        m.finalize().into_bytes().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 Appendix B test secret (SHA-1).
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc6238_vectors_6_digits() {
        // RFC lists 8-digit codes; the 6-digit code is the last six digits.
        for (t, code8) in [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
        ] {
            assert_eq!(code_at(RFC_SECRET, t).unwrap(), &code8[2..]);
        }
    }

    #[test]
    fn verify_window_and_replay() {
        let t = 1_700_000_000u64;
        let step = t / STEP_SECS;
        let code = code_at(RFC_SECRET, t).unwrap();
        assert_eq!(verify(RFC_SECRET, &code, t, None).unwrap(), Some(step));
        // ±1 step of drift is fine, 2 steps is not.
        assert_eq!(verify(RFC_SECRET, &code, t + 30, None).unwrap(), Some(step));
        assert_eq!(verify(RFC_SECRET, &code, t - 30, None).unwrap(), Some(step));
        assert_eq!(verify(RFC_SECRET, &code, t + 61, None).unwrap(), None);
        // Replay of the same or an older step is refused.
        assert_eq!(verify(RFC_SECRET, &code, t, Some(step)).unwrap(), None);
        assert_eq!(verify(RFC_SECRET, &code, t, Some(step + 1)).unwrap(), None);
        assert_eq!(
            verify(RFC_SECRET, &code, t, Some(step - 1)).unwrap(),
            Some(step)
        );
        // Formatting is tolerated; garbage is not.
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(verify(RFC_SECRET, &spaced, t, None).unwrap(), Some(step));
        assert_eq!(verify(RFC_SECRET, "12345", t, None).unwrap(), None);
        assert_eq!(verify(RFC_SECRET, "abcdef", t, None).unwrap(), None);
    }

    #[test]
    fn otpauth_url_shape() {
        let s = generate_secret().unwrap();
        let url = otpauth_url(&s, "Modula Deploy", "ada@example.com");
        assert!(url.starts_with("otpauth://totp/Modula%20Deploy:ada%40example.com?secret="));
        assert!(url.contains("&issuer=Modula%20Deploy&algorithm=SHA1&digits=6&period=30"));
        assert_eq!(secret_base32(&s).len(), 32);
    }

    #[test]
    fn backup_codes() {
        let codes = backup::generate().unwrap();
        assert_eq!(codes.len(), backup::COUNT);
        let pepper = [7u8; 32];
        for c in &codes {
            assert_eq!(c.len(), 11);
            let n = backup::normalize(c).unwrap();
            assert_eq!(backup::normalize(&c.to_uppercase()).unwrap(), n);
            assert_eq!(backup::normalize(&c.replace('-', " ")).unwrap(), n);
            assert_eq!(backup::hash(&pepper, &n), backup::hash(&pepper, &n));
            assert_ne!(backup::hash(&pepper, &n), backup::hash(&[8u8; 32], &n));
        }
        assert_eq!(
            backup::normalize("oooo0-iiill").unwrap(),
            "00000-11111".replace('-', "")
        );
        assert!(backup::normalize("short").is_none());
        assert!(
            backup::normalize("uuuuu-uuuuu").is_none(),
            "u is not in the alphabet"
        );
        assert!(backup::normalize("ééééé-ééééé").is_none());
    }
}
