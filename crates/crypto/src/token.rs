//! Opaque bearer tokens: `<prefix>_<43 base64url chars>` (256 bits of entropy).
//!
//! Only the SHA-256 hash is stored. Tokens are high-entropy, so a fast hash is
//! sufficient (no need for a password KDF), and lookups are by hash.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::SecretString;
use sha2::{Digest, Sha256};

use crate::{Result, random_bytes};

const ENTROPY_BYTES: usize = 32;
const ENCODED_LEN: usize = 43;

/// Well-known prefixes. They make leaked tokens greppable by secret scanners.
pub mod prefix {
    pub const SESSION: &str = "mcs";
    pub const MFA_PENDING: &str = "mcm";
    pub const SETUP: &str = "mcsetup";
    pub const API: &str = "mcp";
    /// Personal API tokens for the `modula` CLI and automation.
    pub const API_TOKEN: &str = "mdt";
    pub const NODE_JOIN: &str = "mcj";
    /// Workspace invite links.
    pub const INVITE: &str = "mci";
    /// The secret half of a browser `modula login` (polled for the API token).
    pub const CLI_LOGIN: &str = "mdc";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash(pub [u8; 32]);

impl TokenHash {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub struct GeneratedToken {
    pub plaintext: SecretString,
    pub hash: TokenHash,
}

pub fn generate(prefix: &str) -> Result<GeneratedToken> {
    let raw = random_bytes::<ENTROPY_BYTES>()?;
    let plaintext = format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(raw));
    let hash = hash(&plaintext);
    Ok(GeneratedToken {
        plaintext: SecretString::from(plaintext),
        hash,
    })
}

pub fn hash(token: &str) -> TokenHash {
    let mut h = Sha256::new();
    h.update(b"modula/token/v1\0");
    h.update(token.as_bytes());
    TokenHash(h.finalize().into())
}

/// Cheap syntactic check before touching the database.
pub fn is_well_formed(token: &str, expected_prefix: &str) -> bool {
    let Some(body) = token
        .strip_prefix(expected_prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return false;
    };
    body.len() == ENCODED_LEN
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn generate_and_hash() {
        let t = generate(prefix::SESSION).unwrap();
        let s = t.plaintext.expose_secret();
        assert!(s.starts_with("mcs_"));
        assert!(is_well_formed(s, prefix::SESSION));
        assert_eq!(hash(s), t.hash);
        assert_ne!(generate(prefix::SESSION).unwrap().hash, t.hash);
    }

    #[test]
    fn well_formed_rejects_wrong_shapes() {
        let t = generate(prefix::API).unwrap();
        let s = t.plaintext.expose_secret();
        assert!(!is_well_formed(s, prefix::SESSION));
        assert!(!is_well_formed("mcp_short", prefix::API));
        assert!(!is_well_formed(&format!("{s}x"), prefix::API));
        assert!(!is_well_formed(&s.replace('_', "."), prefix::API));
        // A different prefix that merely starts the same way must not match.
        let setup = generate(prefix::SETUP).unwrap();
        assert!(!is_well_formed(
            setup.plaintext.expose_secret(),
            prefix::SESSION
        ));
    }
}
