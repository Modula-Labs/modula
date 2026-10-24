//! Source archives uploaded with `modula up`: where they are kept, the
//! signed download a node gets while building one, and retention.
//!
//! An upload is a gzipped tarball in `uploads_dir/<id>.tar.gz` plus a
//! `source_uploads` row (sha256, size). Clients can upload but never download:
//! only the desired state handed to the node building a deployment of the
//! upload carries a token for it, and the download endpoint checks that the
//! deployment is still building that upload.

use std::path::PathBuf;

use hmac::{Hmac, KeyInit, Mac};
use modula_id::Id;
use sha2::Sha256;
use time::OffsetDateTime;

use crate::error::AppResult;
use crate::keyring::Keyring;
use crate::state::AppState;

/// The download path nodes fetch (token in `Authorization: Bearer …`).
pub const DOWNLOAD_PATH: &str = "/api/build-source";

/// A download token is valid this long after its deployment was created
/// (and only while the deployment is building).
const TOKEN_TTL: time::Duration = time::Duration::hours(6);

/// Uploads nothing uses are deleted after this long.
pub const RETENTION: time::Duration = time::Duration::hours(24);

/// Uploads used by any of an instance's newest deployments are kept (so a
/// redeploy or rollback can rebuild them), however old.
pub const KEEP_NEWEST_DEPLOYMENTS: i64 = 5;

/// Uploads in flight per workspace at once.
pub const MAX_CONCURRENT_PER_WORKSPACE: u32 = 2;

/// Where an upload's archive lives.
pub fn file(state: &AppState, id: Id) -> PathBuf {
    state.cfg.uploads_dir.join(format!("{id}.tar.gz"))
}

/// Where an upload is written before it is complete and checked.
pub fn partial(state: &AppState, id: Id) -> PathBuf {
    state.cfg.uploads_dir.join(format!("{id}.part"))
}

fn mac(keys: &Keyring, payload: &str) -> Hmac<Sha256> {
    let key = keys.derive_from_master("build-source-download");
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key[..]).expect("any key length");
    mac.update(payload.as_bytes());
    mac
}

/// `<deployment>.<upload>.<expiry>.<hex hmac>`. Deterministic for a
/// deployment, so re-sent desired states don't change the build spec.
pub fn token(keys: &Keyring, deployment: Id, upload: Id, created_at: OffsetDateTime) -> String {
    let expires = (created_at + TOKEN_TTL).unix_timestamp();
    let payload = format!("{deployment}.{upload}.{expires}");
    let sig = hex::encode(mac(keys, &payload).finalize().into_bytes());
    format!("{payload}.{sig}")
}

/// The (deployment, upload) a token was issued for, if it is genuine and unexpired.
pub fn verify(keys: &Keyring, token: &str) -> Option<(Id, Id)> {
    let (payload, sig) = token.rsplit_once('.')?;
    let sig = hex::decode(sig).ok()?;
    mac(keys, payload).verify_slice(&sig).ok()?;
    let mut parts = payload.split('.');
    let (deployment, upload, expires) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let expires: i64 = expires.parse().ok()?;
    (OffsetDateTime::now_utc().unix_timestamp() < expires)
        .then_some((deployment.parse().ok()?, upload.parse().ok()?))
}

/// Holds one of a workspace's upload slots; released on drop.
pub struct Slot {
    state: AppState,
    workspace: Id,
}

impl Slot {
    /// `None` when the workspace already has the maximum uploads in flight.
    pub fn acquire(state: &AppState, workspace: Id) -> Option<Self> {
        let mut map = crate::sync::lock(&state.uploads_in_flight);
        let n = map.entry(workspace).or_insert(0);
        if *n >= MAX_CONCURRENT_PER_WORKSPACE {
            return None;
        }
        *n += 1;
        Some(Self {
            state: state.clone(),
            workspace,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut map = crate::sync::lock(&self.state.uploads_in_flight);
        if let Some(n) = map.get_mut(&self.workspace) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.workspace);
            }
        }
    }
}

/// Retention: delete the archives of uploads older than [`RETENTION`] that no
/// live deployment and none of their instance's [`KEEP_NEWEST_DEPLOYMENTS`]
/// newest deployments use (the row stays, marked purged), then any file in the
/// directory without a kept row (deleted instances, abandoned partial
/// uploads). Returns how many archives were deleted.
pub async fn purge(state: &AppState) -> AppResult<u64> {
    let expired: Vec<Id> = sqlx::query_scalar(
        "UPDATE source_uploads u SET purged_at = now()
         WHERE u.purged_at IS NULL AND u.created_at < now() - make_interval(secs => $1)
           AND NOT EXISTS (
               SELECT 1 FROM deployments d WHERE d.upload_id = u.id
                 AND (d.status IN ('queued', 'building', 'deploying', 'active', 'sleeping', 'crashed')
                      OR d.id IN (SELECT n.id FROM deployments n
                                  WHERE n.service_instance_id = d.service_instance_id
                                  ORDER BY n.created_at DESC LIMIT $2)))
         RETURNING u.id",
    )
    .bind(RETENTION.as_seconds_f64())
    .bind(KEEP_NEWEST_DEPLOYMENTS)
    .fetch_all(&state.db)
    .await?;
    let mut deleted = 0;
    for id in &expired {
        if tokio::fs::remove_file(file(state, *id)).await.is_ok() {
            deleted += 1;
        }
    }

    // Strays: files whose row is gone or purged, after an hour (an upload in
    // progress is a `.part` file younger than that).
    let Ok(mut dir) = tokio::fs::read_dir(&state.cfg.uploads_dir).await else {
        return Ok(deleted);
    };
    let kept: Vec<Id> = sqlx::query_scalar("SELECT id FROM source_uploads WHERE purged_at IS NULL")
        .fetch_all(&state.db)
        .await?;
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.split('.').next())
            .and_then(|n| n.parse::<Id>().ok())
        else {
            continue;
        };
        let complete = name.to_str().is_some_and(|n| n.ends_with(".tar.gz"));
        if complete && kept.contains(&id) {
            continue;
        }
        let old = entry
            .metadata()
            .await
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < hour_ago);
        if old && tokio::fs::remove_file(entry.path()).await.is_ok() {
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
        let state = Keyring::new(modula_crypto::envelope::MasterKey::generate().unwrap());
        let (d, u) = (Id::new(), Id::new());
        let now = OffsetDateTime::now_utc();
        let t = token(&state, d, u, now);
        assert_eq!(t, token(&state, d, u, now), "deterministic");
        assert_eq!(verify(&state, &t), Some((d, u)));
        // Tampering with any part breaks the signature.
        let other = t.replacen(&d.to_string(), &Id::new().to_string(), 1);
        assert_eq!(verify(&state, &other), None);
        assert_eq!(verify(&state, &format!("{t}0")), None);
        assert_eq!(verify(&state, "garbage"), None);
        // Issued for a deployment created long ago: expired.
        let old = token(&state, d, u, now - time::Duration::hours(7));
        assert_eq!(verify(&state, &old), None);
    }
}
