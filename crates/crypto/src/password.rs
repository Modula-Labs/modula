//! Password hashing with argon2id: m=64 MiB, t=3, p=1 for new hashes (above
//! OWASP's 19 MiB/t=2 floor). Verification takes the parameters from the PHC
//! string, so a hash verifies whatever parameters it was made with.

use std::sync::LazyLock;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::{Error, Result};

pub const MIN_LEN: usize = 10;
pub const MAX_LEN: usize = 256;

/// Verified against when the user does not exist, so both paths cost the same.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    hash("modula-dummy-password-for-timing").expect("hashing a constant cannot fail")
});

/// Memory cost of new hashes, in KiB.
const M_COST: u32 = 64 * 1024;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

fn hasher() -> Argon2<'static> {
    let params = Params::new(M_COST, T_COST, P_COST, None).expect("valid argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash(password: &str) -> Result<String> {
    hasher()
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
        assert!(h.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"), "{h}");
        assert!(verify("correct horse battery", &h));
        assert!(!verify("wrong horse battery", &h));
        assert!(!verify("anything", "not-a-phc-string"));
        assert!(!verify_dummy("anything"));
        // A hash with other parameters (argon2's defaults) verifies too.
        let old = Argon2::default()
            .hash_password(b"correct horse battery")
            .unwrap()
            .to_string();
        assert!(old.contains("m=19456,t=2,p=1"), "{old}");
        assert!(verify("correct horse battery", &old));
    }

    #[test]
    fn policy() {
        assert!(validate_policy("short").is_err());
        assert!(validate_policy("long enough pw").is_ok());
        assert!(validate_policy(&"x".repeat(300)).is_err());
    }
}
