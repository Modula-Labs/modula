//! Envelope encryption.
//!
//! Layout of every sealed blob: `version (1) || nonce (24) || ciphertext+tag`.
//! Cipher: XChaCha20-Poly1305 (random 192-bit nonces are safe without counters).
//!
//! AAD binds a ciphertext to where it lives, so a value copied into another
//! row, column, table or workspace fails to decrypt instead of being accepted.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use modula_id::Id;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{Error, Result, random_bytes};

pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const VERSION: u8 = 1;
const DEK_AAD_DOMAIN: &[u8] = b"modula/dek/v1";
const FIELD_AAD_DOMAIN: &[u8] = b"modula/field/v1";

/// Scope used for instance-level secrets that belong to no workspace
/// (e.g. the self-hosted GitHub App private key).
pub const INSTANCE_SCOPE: Id = Id::ZERO;

/// Where a sealed field lives. Every part is length-prefixed in the AAD.
#[derive(Debug, Clone, Copy)]
pub struct FieldAad<'a> {
    pub scope: Id,
    pub table: &'a str,
    pub column: &'a str,
    pub row: &'a str,
}

impl FieldAad<'_> {
    fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(64 + self.table.len() + self.column.len() + self.row.len());
        push_part(&mut out, FIELD_AAD_DOMAIN);
        push_part(&mut out, self.scope.as_bytes());
        push_part(&mut out, self.table.as_bytes());
        push_part(&mut out, self.column.as_bytes());
        push_part(&mut out, self.row.as_bytes());
        out
    }
}

fn push_part(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u32).to_be_bytes());
    out.extend_from_slice(part);
}

fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce_bytes = random_bytes::<NONCE_LEN>()?;
    let nonce = XNonce::from(nonce_bytes);
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Decrypt)?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(key: &[u8; KEY_LEN], sealed: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let (&version, rest) = sealed.split_first().ok_or(Error::Malformed)?;
    if version != VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    if rest.len() < NONCE_LEN + 16 {
        return Err(Error::Malformed);
    }
    let (nonce_bytes, ct) = rest.split_at(NONCE_LEN);
    let nonce = XNonce::try_from(nonce_bytes).map_err(|_| Error::Malformed)?;
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(&nonce, Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| Error::Decrypt)
}

/// The key-encryption key. Loaded from a 0600 file; never stored in the database.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; KEY_LEN]);

impl MasterKey {
    pub fn generate() -> Result<Self> {
        Ok(Self(random_bytes()?))
    }

    /// Parse the base64 form written by [`MasterKey::encode`] (surrounding whitespace ignored).
    pub fn from_encoded(encoded: &str) -> Result<Self> {
        let bytes = Zeroizing::new(
            STANDARD
                .decode(encoded.trim())
                .map_err(|_| Error::InvalidKey)?,
        );
        let arr: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| Error::InvalidKey)?;
        Ok(Self(arr))
    }

    pub fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.0))
    }

    /// Stable, non-secret identifier, stored next to wrapped keys to support rotation.
    pub fn id(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"modula/kek-id/v1");
        h.update(self.0);
        format!("kek_{}", hex::encode(&h.finalize()[..8]))
    }

    pub fn wrap(&self, dek: &DataKey, scope: Id) -> Result<Vec<u8>> {
        seal(&self.0, &dek.0, &dek_aad(scope))
    }

    pub fn unwrap(&self, wrapped: &[u8], scope: Id) -> Result<DataKey> {
        let plain = open(&self.0, wrapped, &dek_aad(scope))?;
        let arr: [u8; KEY_LEN] = plain.as_slice().try_into().map_err(|_| Error::Malformed)?;
        Ok(DataKey(arr))
    }
}

fn dek_aad(scope: Id) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    push_part(&mut out, DEK_AAD_DOMAIN);
    push_part(&mut out, scope.as_bytes());
    out
}

/// A per-scope (workspace or instance) data-encryption key.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct DataKey([u8; KEY_LEN]);

impl DataKey {
    pub fn generate() -> Result<Self> {
        Ok(Self(random_bytes()?))
    }

    /// Seal a field. `aad.scope` must be the scope this key belongs to.
    pub fn seal(&self, plaintext: &[u8], aad: &FieldAad<'_>) -> Result<Vec<u8>> {
        seal(&self.0, plaintext, &aad.encode())
    }

    pub fn open(&self, sealed: &[u8], aad: &FieldAad<'_>) -> Result<Zeroizing<Vec<u8>>> {
        open(&self.0, sealed, &aad.encode())
    }

    /// Derive a purpose-bound sub-key (e.g. the per-environment `secrets-hash` HMAC key).
    pub fn derive(&self, purpose: &str) -> [u8; KEY_LEN] {
        let mut h = Sha256::new();
        h.update(b"modula/derive/v1");
        h.update((purpose.len() as u32).to_be_bytes());
        h.update(purpose.as_bytes());
        h.update(self.0);
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aad(row: &str) -> FieldAad<'_> {
        FieldAad {
            scope: "0000000000000007".parse::<Id>().unwrap(),
            table: "variables",
            column: "value",
            row,
        }
    }

    #[test]
    fn field_round_trip() {
        let dek = DataKey::generate().unwrap();
        let sealed = dek.seal(b"postgres://secret", &aad("r1")).unwrap();
        assert_eq!(sealed[0], VERSION);
        assert_eq!(
            &*dek.open(&sealed, &aad("r1")).unwrap(),
            b"postgres://secret"
        );
    }

    #[test]
    fn ciphertext_is_bound_to_its_row() {
        let dek = DataKey::generate().unwrap();
        let sealed = dek.seal(b"v", &aad("r1")).unwrap();
        assert!(matches!(dek.open(&sealed, &aad("r2")), Err(Error::Decrypt)));
        let other_col = FieldAad {
            column: "name",
            ..aad("r1")
        };
        assert!(dek.open(&sealed, &other_col).is_err());
        let other_scope = FieldAad {
            scope: "0000000000000008".parse::<Id>().unwrap(),
            ..aad("r1")
        };
        assert!(dek.open(&sealed, &other_scope).is_err());
    }

    #[test]
    fn aad_parts_are_unambiguous() {
        // ("ab","c") must not collide with ("a","bc").
        let a = FieldAad {
            scope: Id::ZERO,
            table: "ab",
            column: "c",
            row: "x",
        }
        .encode();
        let b = FieldAad {
            scope: Id::ZERO,
            table: "a",
            column: "bc",
            row: "x",
        }
        .encode();
        assert_ne!(a, b);
    }

    #[test]
    fn tamper_and_version_checks() {
        let dek = DataKey::generate().unwrap();
        let mut sealed = dek.seal(b"value", &aad("r")).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(matches!(dek.open(&sealed, &aad("r")), Err(Error::Decrypt)));
        sealed[0] = 9;
        assert!(matches!(
            dek.open(&sealed, &aad("r")),
            Err(Error::UnsupportedVersion(9))
        ));
        assert!(matches!(
            dek.open(&[1, 2, 3], &aad("r")),
            Err(Error::Malformed)
        ));
        assert!(matches!(dek.open(&[], &aad("r")), Err(Error::Malformed)));
    }

    #[test]
    fn nonces_are_unique() {
        let dek = DataKey::generate().unwrap();
        let a = dek.seal(b"same", &aad("r")).unwrap();
        let b = dek.seal(b"same", &aad("r")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn wrap_unwrap_bound_to_scope() {
        let kek = MasterKey::generate().unwrap();
        let dek = DataKey::generate().unwrap();
        let ws = "0000000000000001".parse::<Id>().unwrap();
        let wrapped = kek.wrap(&dek, ws).unwrap();
        let back = kek.unwrap(&wrapped, ws).unwrap();
        assert_eq!(back.0, dek.0);
        assert!(
            kek.unwrap(&wrapped, "0000000000000002".parse::<Id>().unwrap())
                .is_err()
        );
        let other = MasterKey::generate().unwrap();
        assert!(other.unwrap(&wrapped, ws).is_err());
    }

    #[test]
    fn master_key_encoding() {
        let kek = MasterKey::generate().unwrap();
        let enc = kek.encode();
        let back = MasterKey::from_encoded(&format!("  {}\n", &*enc)).unwrap();
        assert_eq!(back.0, kek.0);
        assert_eq!(back.id(), kek.id());
        assert!(kek.id().starts_with("kek_"));
        assert!(MasterKey::from_encoded("dG9vLXNob3J0").is_err());
        assert!(MasterKey::from_encoded("not base64 !!").is_err());
    }

    #[test]
    fn derive_is_purpose_bound() {
        let dek = DataKey::generate().unwrap();
        assert_ne!(
            dek.derive("secrets-hash:env1"),
            dek.derive("secrets-hash:env2")
        );
        assert_eq!(dek.derive("x"), dek.derive("x"));
    }
}
