//! Password hashing with argon2id (OWASP defaults: m=19 MiB, t=2, p=1).

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

use crate::{Error, Result};

pub const MIN_LEN: usize = 10;
pub const MAX_LEN: usize = 256;

/// Verified against when the user does not exist, so both paths cost the same.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    hash("modula-dummy-password-for-timing").expect("hashing a constant cannot fail")
});

pub fn hash(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|_| Error::PasswordHash)
}

/// Returns false for a wrong password or an unparseable stored hash.
pub fn verify(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Burn the same work as a real verification; always returns false.
pub fn verify_dummy(password: &str) -> bool {
    let _ = verify(password, &DUMMY_HASH);
    false
}

/// Length policy only (NIST 800-63B: length over composition rules).
pub fn validate_policy(password: &str) -> std::result::Result<(), &'static str> {
    let n = password.chars().count();
    if n < MIN_LEN {
        return Err("password must be at least 10 characters");
    }
    if password.len() > MAX_LEN {
        return Err("password must be at most 256 bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse battery", &h));
        assert!(!verify("wrong horse battery", &h));
        assert!(!verify("anything", "not-a-phc-string"));
        assert!(!verify_dummy("anything"));
    }

    #[test]
    fn policy() {
        assert!(validate_policy("short").is_err());
        assert!(validate_policy("long enough pw").is_ok());
        assert!(validate_policy(&"x".repeat(300)).is_err());
    }
}
