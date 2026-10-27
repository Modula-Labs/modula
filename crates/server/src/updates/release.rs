//! What the newest published release is, checked the way install.sh checks
//! binaries: `SHA256SUMS` must carry a valid Ed25519 signature from the release
//! key, and the `VERSION` file must match its line in it.

use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use sha2::{Digest, Sha256};
use url::Url;

/// deploy/release.pub.pem, the key install.sh embeds too.
const RELEASE_PUBKEY_PEM: &str = include_str!("../../../../deploy/release.pub.pem");
/// DER prefix of an Ed25519 SubjectPublicKeyInfo; the raw key follows.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
/// Upper bounds on what is downloaded (the files are tiny).
const MAX_SUMS: usize = 64 * 1024;
const MAX_VERSION: usize = 128;

/// An Ed25519 public key from base64 SubjectPublicKeyInfo (a PEM body).
pub fn pubkey_from_spki(b64: &str) -> anyhow::Result<[u8; 32]> {
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("not base64")?;
    let key = der
        .strip_prefix(&ED25519_SPKI_PREFIX[..])
        .context("not an Ed25519 public key")?;
    key.try_into().context("not an Ed25519 public key")
}

pub fn embedded_pubkey() -> [u8; 32] {
    let body: String = RELEASE_PUBKEY_PEM
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    pubkey_from_spki(&body).expect("deploy/release.pub.pem is an Ed25519 key")
}

/// The release version (`0.2.0`), from files of one release: `sums` must be
/// signed by `pubkey`, and `version_file` must be listed in it as `VERSION`.
pub fn verify(
    sums: &[u8],
    sig: &[u8],
    version_file: &[u8],
    pubkey: &[u8; 32],
) -> anyhow::Result<semver::Version> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pubkey)
        .verify(sums, sig)
        .map_err(|_| anyhow::anyhow!("SHA256SUMS is not signed by the release key"))?;
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not text")?;
    let want = hex::encode(Sha256::digest(version_file));
    let listed = sums.lines().any(|l| {
        let mut parts = l.split_whitespace();
        parts.next() == Some(want.as_str())
            && parts.next().map(|f| f.trim_start_matches('*')) == Some("VERSION")
    });
    if !listed {
        bail!("VERSION does not match SHA256SUMS");
    }
    let v = std::str::from_utf8(version_file)
        .context("VERSION is not text")?
        .trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    semver::Version::parse(v).with_context(|| format!("VERSION is not a version: {v:?}"))
}

/// `current` and `candidate` compare as versions; a candidate that doesn't
/// parse is never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
}

/// Where a release's files (and notes, when published) live.
pub fn release_page(base: &Url, version: &str) -> String {
    format!(
        "{}/download/v{version}/",
        base.as_str().trim_end_matches('/')
    )
}

/// The newest published release. `Ok(None)` when there is none (no `VERSION`
/// file at the latest-release URL).
pub async fn fetch_latest(
    http: &reqwest::Client,
    base: &Url,
    pubkey: &[u8; 32],
) -> anyhow::Result<Option<semver::Version>> {
    let url = |file: &str| {
        format!(
            "{}/latest/download/{file}",
            base.as_str().trim_end_matches('/')
        )
    };
    let Some(version) = get(http, &url("VERSION"), MAX_VERSION).await? else {
        return Ok(None);
    };
    let sums = get(http, &url("SHA256SUMS"), MAX_SUMS)
        .await?
        .context("the release has no SHA256SUMS")?;
    let sig = get(http, &url("SHA256SUMS.sig"), 1024)
        .await?
        .context("the release has no SHA256SUMS.sig")?;
    verify(&sums, &sig, &version, pubkey).map(Some)
}

/// GET a small file; `None` on 404.
async fn get(http: &reqwest::Client, url: &str, max: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    let body = resp.bytes().await.context("download interrupted")?;
    if body.len() > max {
        bail!("{url} is larger than expected");
    }
    Ok(Some(body.to_vec()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::KeyPair;

    /// A throwaway release key: (signing key, raw public key).
    pub(crate) fn test_key() -> (ring::signature::Ed25519KeyPair, [u8; 32]) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = kp.public_key().as_ref().try_into().unwrap();
        (kp, public)
    }

    /// SHA256SUMS listing `VERSION` (plus a binary), and its signature.
    pub(crate) fn signed_release(
        key: &ring::signature::Ed25519KeyPair,
        version: &str,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let version_file = format!("{version}\n").into_bytes();
        let sums = format!(
            "{}  modula-server-linux-arm64\n{}  VERSION\n",
            hex::encode(Sha256::digest(b"binary")),
            hex::encode(Sha256::digest(&version_file))
        )
        .into_bytes();
        let sig = key.sign(&sums).as_ref().to_vec();
        (sums, sig, version_file)
    }

    #[test]
    fn the_embedded_key_parses() {
        let k = embedded_pubkey();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .encode([&ED25519_SPKI_PREFIX[..], &k].concat()),
            "MCowBQYDK2VwAyEAVl1B0m1qDZY+gGHSLVry769Tl1KmGKKFhFiav9ccXCE="
        );
        assert!(pubkey_from_spki("bm90IGEga2V5").is_err());
    }

    #[test]
    fn verifies_signature_and_version() {
        let (key, public) = test_key();
        let (sums, sig, version) = signed_release(&key, "0.3.1");
        assert_eq!(
            verify(&sums, &sig, &version, &public).unwrap().to_string(),
            "0.3.1"
        );

        // A different key, a tampered list, a VERSION not in the list.
        let (_, other) = test_key();
        assert!(verify(&sums, &sig, &version, &other).is_err());
        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        assert!(verify(&tampered, &sig, &version, &public).is_err());
        assert!(verify(&sums, &sig, b"9.9.9\n", &public).is_err());
        // Signed and listed, but not a version.
        let (sums, sig, version) = signed_release(&key, "latest");
        assert!(verify(&sums, &sig, &version, &public).is_err());
    }

    #[test]
    fn version_order() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(
            release_page(
                &"https://downloads.example.com/releases/".parse().unwrap(),
                "0.2.0"
            ),
            "https://downloads.example.com/releases/download/v0.2.0/"
        );
    }
}
