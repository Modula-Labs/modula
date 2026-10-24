//! Build output for static sites hosted elsewhere (`config.hosting`): the
//! node that built a deployment uploads its output directory here as a
//! gzipped tarball, and the server publishes it (see
//! `integrations::cloudflare::publish`).
//!
//! The node never holds hosting credentials: it only gets a signed,
//! deployment-bound upload token in its desired state (like the source
//! download token in [`super::upload`]), valid while the deployment is
//! building. The server keeps the archive until it is published.

use std::path::PathBuf;

use hmac::{Hmac, KeyInit, Mac};
use modula_id::Id;
use sha2::Sha256;
use time::OffsetDateTime;

use crate::keyring::Keyring;
use crate::state::AppState;

/// The path nodes PUT build output to (token in `Authorization: Bearer …`).
pub const UPLOAD_PATH: &str = "/api/build-output";

/// An upload token is valid this long after its deployment was created (and
/// only while the deployment is building).
const TOKEN_TTL: time::Duration = time::Duration::hours(6);

/// Where outputs are kept until published.
pub fn dir(state: &AppState) -> PathBuf {
    state.cfg.uploads_dir.join("outputs")
}

/// A deployment's received output archive.
pub fn file(state: &AppState, deployment: Id) -> PathBuf {
    dir(state).join(format!("{deployment}.tar.gz"))
}

/// Where an output is written while it is being received.
pub fn partial(state: &AppState, deployment: Id) -> PathBuf {
    dir(state).join(format!("{deployment}.part"))
}

fn mac(keys: &Keyring, payload: &str) -> Hmac<Sha256> {
    let key = keys.derive_from_master("build-output-upload");
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key[..]).expect("any key length");
    mac.update(payload.as_bytes());
    mac
}

/// `<deployment>.<expiry>.<hex hmac>`. Deterministic for a deployment, so
/// re-sent desired states don't change the build spec.
pub fn token(keys: &Keyring, deployment: Id, created_at: OffsetDateTime) -> String {
    let expires = (created_at + TOKEN_TTL).unix_timestamp();
    let payload = format!("{deployment}.{expires}");
    let sig = hex::encode(mac(keys, &payload).finalize().into_bytes());
    format!("{payload}.{sig}")
}

/// The deployment a token was issued for, if it is genuine and unexpired.
pub fn verify(keys: &Keyring, token: &str) -> Option<Id> {
    let (payload, sig) = token.rsplit_once('.')?;
    let sig = hex::decode(sig).ok()?;
    mac(keys, payload).verify_slice(&sig).ok()?;
    let (deployment, expires) = payload.split_once('.')?;
    let expires: i64 = expires.parse().ok()?;
    (OffsetDateTime::now_utc().unix_timestamp() < expires).then_some(deployment.parse().ok()?)
}

/// Deletes a deployment's archive (published, failed or superseded).
pub async fn discard(state: &AppState, deployment: Id) {
    let _ = tokio::fs::remove_file(file(state, deployment)).await;
    let _ = tokio::fs::remove_file(partial(state, deployment)).await;
}

/// Retention: archives of deployments that are no longer building or being
/// published (cancelled or failed mid-way), after an hour.
pub async fn purge(state: &AppState) -> crate::error::AppResult<u64> {
    let Ok(mut entries) = tokio::fs::read_dir(dir(state)).await else {
        return Ok(0);
    };
    let pending: Vec<Id> = sqlx::query_scalar(
        "SELECT id FROM deployments WHERE status IN ('building', 'deploying')
           AND config->'hosting' IS NOT NULL",
    )
    .fetch_all(&state.db)
    .await?;
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut deleted = 0;
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name();
        let id = name
            .to_str()
            .and_then(|n| n.split('.').next())
            .and_then(|n| n.parse::<Id>().ok());
        if id.is_some_and(|id| pending.contains(&id)) {
            continue;
        }
        let old = e
            .metadata()
            .await
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < hour_ago);
        if old && tokio::fs::remove_file(e.path()).await.is_ok() {
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_verify_and_expire() {
        let keys = Keyring::new(modula_crypto::envelope::MasterKey::generate().unwrap());
        let d = Id::new();
        let now = OffsetDateTime::now_utc();
        let t = token(&keys, d, now);
        assert_eq!(t, token(&keys, d, now), "deterministic");
        assert_eq!(verify(&keys, &t), Some(d));
        let other = t.replacen(&d.to_string(), &Id::new().to_string(), 1);
        assert_eq!(verify(&keys, &other), None);
        assert_eq!(verify(&keys, "garbage"), None);
        // Not interchangeable with source download tokens.
        let source = super::super::upload::token(&keys, d, Id::new(), now);
        assert_eq!(verify(&keys, &source), None);
        let old = token(&keys, d, now - time::Duration::hours(7));
        assert_eq!(verify(&keys, &old), None);
    }
}
