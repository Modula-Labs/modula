//! Source uploads for `modula up`: the CLI streams a gzipped tarball
//! here, then creates a deployment from it (see `deployments::create`).
//! Nodes download it through the signed [`download`] endpoint; clients never can.

use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use modula_id::Id;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::upload::{self, Slot};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;

/// The SHA-256 the client computed (hex); a mismatch rejects the upload.
pub const SHA256_HEADER: &str = "x-content-sha256";

/// Uploads per user per hour.
const RATE: Limit = Limit::per(60, Duration::from_secs(3600));

/// Bytes a workspace may upload per rolling 24 hours, across all its users
/// (the per-user rate alone lets many members fill the disk).
pub const WORKSPACE_DAILY_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// A stalled upload is abandoned after this long without data.
const IDLE: Duration = Duration::from_secs(60);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Upload {
    pub id: Id,
    pub sha256: String,
    pub size_bytes: i64,
}

/// `POST /api/service-instances/{id}/uploads` (body: `application/gzip`).
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Json<Upload>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let max = state.cfg.upload_max_bytes;
    let too_large = || {
        AppError::PayloadTooLarge(format!(
            "the upload is larger than this server's {} MB limit",
            max / 1024 / 1024
        ))
    };
    let gzip = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v.trim(), "application/gzip" | "application/x-gzip"));
    if !gzip {
        return Err(AppError::bad_request(
            "the upload must be a gzipped tarball (Content-Type: application/gzip)",
        ));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        return Err(too_large());
    }
    let expected = match headers.get(SHA256_HEADER) {
        None => None,
        Some(v) => Some(
            v.to_str()
                .ok()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| AppError::bad_request("invalid x-content-sha256 header"))?,
        ),
    };
    state
        .limiter
        .check("upload", &actor.user_id.to_string(), RATE)
        .await?;
    let _slot = Slot::acquire(&state, workspace).ok_or(AppError::TooManyRequests {
        retry_after_secs: 10,
    })?;
    let used: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(size_bytes), 0)::bigint FROM source_uploads
         WHERE workspace_id = $1 AND created_at > now() - interval '1 day'",
    )
    .bind(workspace)
    .fetch_one(&state.db)
    .await?;
    let remaining = WORKSPACE_DAILY_BYTES.saturating_sub(u64::try_from(used).unwrap_or(0));
    let over_quota = || {
        AppError::PayloadTooLarge(format!(
            "this workspace has used its {} GB of uploads for the last 24 hours; try again later",
            WORKSPACE_DAILY_BYTES / 1024 / 1024 / 1024
        ))
    };
    if remaining == 0 || declared.is_some_and(|n| n > remaining) {
        return Err(over_quota());
    }

    let id = Id::new();
    let part = upload::partial(&state, id);
    tokio::fs::create_dir_all(&state.cfg.uploads_dir)
        .await
        .map_err(anyhow::Error::from)?;
    // Whatever goes wrong below, no partial file is left behind.
    let received = receive(body, &part, max.min(remaining)).await;
    let (sha256, size) = match received {
        Ok(Some(done)) => done,
        Ok(None) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(if remaining < max {
                over_quota()
            } else {
                too_large()
            });
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
    };
    if let Some(want) = &expected
        && *want != sha256
    {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(AppError::bad_request(
            "the upload's checksum does not match x-content-sha256 (corrupted in transit?)",
        ));
    }
    if let Err(e) = tokio::fs::rename(&part, upload::file(&state, id)).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(anyhow::Error::from(e).into());
    }
    let size = i64::try_from(size).map_err(anyhow::Error::from)?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO source_uploads (id, workspace_id, service_instance_id, sha256, size_bytes, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(workspace)
    .bind(instance)
    .bind(&sha256)
    .bind(size)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("deployment.upload", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("source_upload", id)
        .metadata(json!({ "serviceInstance": instance, "sha256": sha256, "sizeBytes": size }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(Upload {
        id,
        sha256,
        size_bytes: size,
    }))
}

/// Stream `body` into `path`, hashing as it goes. `None` once it passes `max`
/// bytes. The first bytes must be a gzip header.
async fn receive(body: Body, path: &std::path::Path, max: u64) -> AppResult<Option<(String, u64)>> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(anyhow::Error::from)?;
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut head: Vec<u8> = Vec::with_capacity(2);
    loop {
        let chunk = match tokio::time::timeout(IDLE, stream.next()).await {
            Err(_) => return Err(AppError::bad_request("the upload stalled")),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(AppError::bad_request("the upload was interrupted")),
            Ok(Some(Ok(c))) => c,
        };
        size += chunk.len() as u64;
        if size > max {
            return Ok(None);
        }
        if head.len() < 2 {
            head.extend(chunk.iter().take(2 - head.len()));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
    }
    if head != [0x1f, 0x8b] {
        return Err(AppError::bad_request(
            "the upload is not gzip data; pack it with `modula up`",
        ));
    }
    file.flush().await.map_err(anyhow::Error::from)?;
    file.sync_all().await.map_err(anyhow::Error::from)?;
    Ok(Some((hex::encode(hasher.finalize()), size)))
}

/// `GET /api/build-source` for nodes: the archive of the upload a deployment
/// is building, authorized by the token in its desired state (`Authorization:
/// Bearer <token>`). Anything else is 401/404.
pub async fn download(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthorized)?;
    let (deployment, upload_id) =
        upload::verify(&state.keys, token.trim()).ok_or(AppError::Unauthorized)?;
    let size: Option<i64> = sqlx::query_scalar(
        "SELECT u.size_bytes FROM deployments d JOIN source_uploads u ON u.id = d.upload_id
         WHERE d.id = $1 AND u.id = $2 AND d.status = 'building' AND u.purged_at IS NULL",
    )
    .bind(deployment)
    .bind(upload_id)
    .fetch_optional(&state.db)
    .await?;
    let size = size.ok_or(AppError::NotFound)?;
    let file = tokio::fs::File::open(upload::file(&state, upload_id))
        .await
        .map_err(|_| AppError::NotFound)?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response())
}
