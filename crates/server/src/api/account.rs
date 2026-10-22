//! The caller's own account: 2FA, backup codes, sessions, password.

use axum::Json;
use axum::extract::State;
use modula_crypto::password;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::mfa;
use crate::auth::session::{self, Actor};
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::validate;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Security {
    pub totp_enabled: bool,
    pub backup_codes_remaining: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub sudo_until: Option<OffsetDateTime>,
}

pub async fn security(State(state): State<AppState>, actor: Actor) -> AppResult<Json<Security>> {
    Ok(Json(Security {
        totp_enabled: mfa::totp_enabled(&state.db, actor.user_id).await?,
        backup_codes_remaining: mfa::backup_codes_remaining(&state.db, actor.user_id).await?,
        sudo_until: actor.sudo_until.filter(|_| actor.sudo_active()),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollResponse {
    pub otpauth_url: String,
    pub secret: String,
}

pub async fn totp_begin(
    State(state): State<AppState>,
    actor: Actor,
) -> AppResult<Json<EnrollResponse>> {
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(actor.user_id)
        .fetch_one(&state.db)
        .await?;
    let e = mfa::begin_enrollment(&state, actor.user_id, &email).await?;
    Ok(Json(EnrollResponse {
        otpauth_url: e.otpauth_url.to_string(),
        secret: e.secret_base32.to_string(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    pub code: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupCodes {
    /// Shown exactly once.
    pub backup_codes: Vec<String>,
}

pub async fn totp_confirm(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<ConfirmRequest>,
) -> AppResult<Json<BackupCodes>> {
    let codes = mfa::confirm_enrollment(&state, actor.user_id, &req.code).await?;
    Entry::new("account.totp_enabled", &meta)
        .actor(&actor)
        .record(&state.db)
        .await?;
    Ok(Json(BackupCodes {
        backup_codes: codes.iter().map(|c| c.to_string()).collect(),
    }))
}

pub async fn totp_disable(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
) -> AppResult<Json<Value>> {
    mfa::disable(&state, actor.user_id).await?;
    Entry::new("account.totp_disabled", &meta)
        .actor(&actor)
        .record(&state.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn backup_codes_regenerate(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
) -> AppResult<Json<BackupCodes>> {
    let codes = mfa::regenerate_backup_codes(&state, actor.user_id).await?;
    Entry::new("account.backup_codes_regenerated", &meta)
        .actor(&actor)
        .record(&state.db)
        .await?;
    Ok(Json(BackupCodes {
        backup_codes: codes.iter().map(|c| c.to_string()).collect(),
    }))
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: Uuid,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    #[sqlx(skip)]
    pub current: bool,
}

pub async fn sessions(
    State(state): State<AppState>,
    actor: Actor,
) -> AppResult<Json<Vec<SessionInfo>>> {
    let mut rows: Vec<SessionInfo> = sqlx::query_as(
        "SELECT id, ip, user_agent, created_at, last_seen_at FROM sessions
         WHERE user_id = $1 AND kind = 'full' AND expires_at > now()
         ORDER BY last_seen_at DESC",
    )
    .bind(actor.user_id)
    .fetch_all(&state.db)
    .await?;
    for r in &mut rows {
        r.current = r.id == actor.session_id;
    }
    Ok(Json(rows))
}

pub async fn session_revoke(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Value>> {
    session::revoke(&state.db, grant.resource.id).await?;
    Entry::new("account.session_revoked", &meta)
        .actor(&actor)
        .target("session", grant.resource.id)
        .record(&state.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PasswordChange {
    pub new_password: SecretString,
}

/// Requires sudo; signs out every other session.
pub async fn change_password(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<PasswordChange>,
) -> AppResult<Json<Value>> {
    validate::password(req.new_password.expose_secret())?;
    let pw = req.new_password;
    let hash = tokio::task::spawn_blocking(move || password::hash(pw.expose_secret()))
        .await
        .map_err(|e| AppError::Internal(e.into()))??;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(actor.user_id)
        .bind(hash)
        .execute(&state.db)
        .await?;
    let revoked = session::revoke_others(&state.db, actor.user_id, actor.session_id).await?;
    Entry::new("account.password_changed", &meta)
        .actor(&actor)
        .metadata(json!({ "sessionsRevoked": revoked }))
        .record(&state.db)
        .await?;
    Ok(Json(json!({ "ok": true, "sessionsRevoked": revoked })))
}
