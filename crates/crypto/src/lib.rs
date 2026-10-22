//! Cryptographic primitives for Modula Cloud.
//!
//! - [`envelope`]: a master key (KEK) wraps per-scope data keys (DEKs); DEKs seal
//!   individual fields with AAD bound to `(scope, table, column, row)`.
//! - [`token`]: high-entropy opaque tokens (sessions, setup, API) stored only as hashes.
//! - [`password`]: argon2id password hashing.

pub mod envelope;
pub mod password;
pub mod token;
pub mod totp;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid key material")]
    InvalidKey,
    #[error("decryption failed")]
    Decrypt,
    #[error("unsupported ciphertext version {0}")]
    UnsupportedVersion(u8),
    #[error("malformed ciphertext")]
    Malformed,
    #[error("os random number generator failed")]
    Rng,
    #[error("password hashing failed")]
    PasswordHash,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fill `N` bytes from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|_| Error::Rng)?;
    Ok(buf)
}

/// Constant-time equality for secrets of possibly different lengths.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}
