//! age X25519 keys. Backups are encrypted to a recipient (`age1…`, public);
//! only the matching identity (`AGE-SECRET-KEY-1…`) decrypts them.

use std::str::FromStr;

use age::secrecy::ExposeSecret;
pub use age::x25519::{Identity, Recipient};
use anyhow::Context;
use zeroize::Zeroizing;

/// A fresh key pair: the recipient and the identity in their text forms.
pub fn generate() -> (String, Zeroizing<String>) {
    let identity = Identity::generate();
    let recipient = identity.to_public().to_string();
    (
        recipient,
        Zeroizing::new(identity.to_string().expose_secret().to_owned()),
    )
}

/// The identity whose secret scalar is `seed`, for keys derived from another
/// secret (the control plane's master key) instead of stored.
pub fn identity_from_seed(seed: &[u8; 32]) -> anyhow::Result<Identity> {
    let hrp = bech32::Hrp::parse("age-secret-key-").expect("valid hrp");
    let encoded = Zeroizing::new(
        bech32::encode::<bech32::Bech32>(hrp, seed)
            .context("cannot encode the identity")?
            .to_uppercase(),
    );
    parse_identity(&encoded)
}

pub fn parse_identity(s: &str) -> anyhow::Result<Identity> {
    Identity::from_str(s.trim()).map_err(|e| anyhow::anyhow!("invalid age identity: {e}"))
}

pub fn parse_recipient(s: &str) -> anyhow::Result<Recipient> {
    Recipient::from_str(s.trim()).map_err(|e| anyhow::anyhow!("invalid age recipient: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pairs_parse_and_match() {
        let (recipient, identity) = generate();
        assert!(recipient.starts_with("age1"));
        assert!(identity.starts_with("AGE-SECRET-KEY-1"));
        let id = parse_identity(&identity).unwrap();
        assert_eq!(id.to_public().to_string(), recipient);
        assert!(parse_recipient(&recipient).is_ok());
        assert!(parse_identity(&recipient).is_err());
    }

    #[test]
    fn seeded_identities_are_deterministic() {
        let a = identity_from_seed(&[7; 32]).unwrap();
        let b = identity_from_seed(&[7; 32]).unwrap();
        let c = identity_from_seed(&[8; 32]).unwrap();
        assert_eq!(a.to_public().to_string(), b.to_public().to_string());
        assert_ne!(a.to_public().to_string(), c.to_public().to_string());
    }
}
