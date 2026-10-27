//! The one-time setup token for a server this CLI creates. Generated here,
//! handed to install.sh (`--setup-token`), which stores only its hash on the
//! server; the CLI then creates the admin with it. Same shape as the tokens
//! modula-server mints itself: `mcsetup_` + 43 base64url characters (256
//! random bits).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

pub const PREFIX: &str = "mcsetup";

pub fn generate() -> anyhow::Result<SecretString> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let token = format!("{PREFIX}_{}", URL_SAFE_NO_PAD.encode(raw));
    raw.zeroize();
    Ok(SecretString::from(token))
}

/// What the server stores (`MODULA_SETUP_TOKEN_HASH`, as install.sh computes
/// it): SHA-256 over `modula/token/v1\0` and the token, in hex.
pub fn hash_hex(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"modula/token/v1\0");
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn tokens_have_the_servers_shape() {
        let t = generate().unwrap();
        let s = t.expose_secret();
        let body = s.strip_prefix("mcsetup_").unwrap();
        assert_eq!(body.len(), 43);
        assert!(
            body.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_ne!(generate().unwrap().expose_secret(), s);
    }

    #[test]
    fn hash_matches_install_sh() {
        // printf 'modula/token/v1\0%s' x | sha256sum
        assert_eq!(
            hash_hex("x"),
            "677b2f45577db1fb15e3876c94149af5fce4a1e90bc40cb96812e19a2769e817"
        );
    }
}
