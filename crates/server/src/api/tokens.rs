//! Personal API tokens (`mdt_…`) for the `modula` CLI and automation.
//!
//! A token authenticates as its user through `Authorization: Bearer`. It never
//! carries sudo (so it can't mint tokens, reveal secrets or delete things that
//! need re-authentication) and, when created for one workspace, reaches
//! nothing outside it. Only SHA-256 hashes are stored; the plaintext is shown
//! once, on creation.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use modula_crypto::token::{self, prefix};
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::audit::Entry;
use crate::auth::session::{Actor, TokenAuth};
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;
use crate::validate;

const CREATE_PER_USER: Limit = Limit::per(20, std::time::Duration::from_secs(3600));
const MAX_TOKENS_PER_USER: i64 = 50;
const MAX_EXPIRY_DAYS: u32 = 3650;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ApiToken {
    pub id: Id,
    pub name: String,
    /// `None` = every workspace the user belongs to.
    pub workspace_id: Option<Id>,
    pub workspace_name: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

macro_rules! select_tokens {
    ($rest:literal) => {
        concat!(
            "SELECT t.id, t.name, t.workspace_id, w.name AS workspace_name, t.last_used_at,
                    t.expires_at, t.created_at
             FROM api_tokens t LEFT JOIN workspaces w ON w.id = t.workspace_id ",
            $rest
        )
    };
}

pub async fn list(State(state): State<AppState>, actor: Actor) -> AppResult<Json<Vec<ApiToken>>> {
    Ok(Json(
        sqlx::query_as(select_tokens!(
            "WHERE t.user_id = $1 ORDER BY t.created_at DESC"
        ))
        .bind(actor.user_id)
        .fetch_all(&state.db)
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateToken {
    pub name: String,
    pub workspace_id: Option<Id>,
    /// `None` = never expires.
    pub expires_in_days: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedToken {
    #[serde(flatten)]
    pub info: ApiToken,
    /// Shown exactly once.
    pub token: String,
}

/// Requires sudo (and therefore a cookie session: tokens never have sudo).
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<CreateToken>,
) -> AppResult<Json<CreatedToken>> {
    state
        .limiter
        .check(
            "api_token_create",
            &actor.user_id.to_string(),
            CREATE_PER_USER,
        )
        .await?;
    let name = validate::display_name(&req.name, "token name")?;
    let expires_at = match req.expires_in_days {
        None => None,
        Some(d @ 1..=MAX_EXPIRY_DAYS) => {
            Some(OffsetDateTime::now_utc() + Duration::days(i64::from(d)))
        }
        Some(_) => {
            return Err(AppError::bad_request(format!(
                "expiresInDays must be between 1 and {MAX_EXPIRY_DAYS}"
            )));
        }
    };
    if let Some(ws) = req.workspace_id {
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2)",
        )
        .bind(ws)
        .bind(actor.user_id)
        .fetch_one(&state.db)
        .await?;
        if !member {
            return Err(AppError::NotFound);
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE user_id = $1")
        .bind(actor.user_id)
        .fetch_one(&state.db)
        .await?;
    if count >= MAX_TOKENS_PER_USER {
        return Err(AppError::Conflict(format!(
            "at most {MAX_TOKENS_PER_USER} API tokens per account; delete unused ones first"
        )));
    }

    let t = token::generate(prefix::API_TOKEN)?;
    let id = Id::new();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, workspace_id, name, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(actor.user_id)
    .bind(req.workspace_id)
    .bind(&name)
    .bind(t.hash.as_bytes())
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    let mut entry = Entry::new("account.token_created", &meta)
        .actor(&actor)
        .target("api_token", id)
        .metadata(json!({ "name": name, "workspace": req.workspace_id, "expiresAt": expires_at }));
    if let Some(ws) = req.workspace_id {
        entry = entry.workspace(ws);
    }
    entry.record(&mut *tx).await?;
    tx.commit().await?;

    let info = sqlx::query_as(select_tokens!("WHERE t.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(CreatedToken {
        info,
        token: t.plaintext.expose_secret().to_owned(),
    }))
}

pub async fn revoke(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Value>> {
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM api_tokens WHERE id = $1")
        .bind(grant.id())
        .execute(&mut *tx)
        .await?;
    Entry::new("account.token_revoked", &meta)
        .actor(&actor)
        .target("api_token", grant.id())
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

/// The credential in an `Authorization` header: `Some(Err)` when the header is
/// present but is not a bearer token, `None` when there is no header.
pub fn bearer(headers: &HeaderMap) -> Option<Result<&str, AppError>> {
    let v = headers.get(header::AUTHORIZATION)?;
    let parsed = v
        .to_str()
        .ok()
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim())
        .ok_or(AppError::Unauthorized);
    Some(parsed)
}

/// Don't write `last_used_at` on every request.
const TOUCH_INTERVAL: Duration = Duration::minutes(1);

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: Id,
    user_id: Id,
    workspace_id: Option<Id>,
    is_instance_admin: bool,
    last_used_at: Option<OffsetDateTime>,
}

/// Resolve a presented API token to its user. Unknown, malformed and expired
/// tokens resolve to `None`.
pub async fn resolve(db: &PgPool, presented: &str) -> AppResult<Option<Actor>> {
    if !token::is_well_formed(presented, prefix::API_TOKEN) {
        return Ok(None);
    }
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT t.id, t.user_id, t.workspace_id, u.is_instance_admin, t.last_used_at
         FROM api_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = $1 AND (t.expires_at IS NULL OR t.expires_at > now())",
    )
    .bind(token::hash(presented).as_bytes())
    .fetch_optional(db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let now = OffsetDateTime::now_utc();
    if row.last_used_at.is_none_or(|t| now - t > TOUCH_INTERVAL) {
        sqlx::query("UPDATE api_tokens SET last_used_at = $2 WHERE id = $1")
            .bind(row.id)
            .bind(now)
            .execute(db)
            .await?;
    }
    Ok(Some(Actor {
        user_id: row.user_id,
        session_id: row.id,
        is_instance_admin: row.is_instance_admin,
        sudo_until: None,
        api_token: Some(TokenAuth {
            id: row.id,
            workspace: row.workspace_id,
        }),
    }))
}

/// A caller authenticated by an API token, resolved once by the CSRF
/// middleware and reused by the route guard.
#[derive(Debug, Clone)]
pub struct BearerActor(pub Actor);
