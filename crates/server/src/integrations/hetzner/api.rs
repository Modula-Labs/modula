//! Workspace Settings → Integrations → Hetzner. The token is verified,
//! sealed with the workspace data key and never returned.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::{Caps, Hetzner, Usage, aad, provider, usage};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::provision::{ProviderError, ProviderLocation, ServerProvider, ServerTypeOffer};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub connected_at: Option<OffsetDateTime>,
    pub connected_by: Option<String>,
    pub caps: Caps,
    pub usage: Usage,
    /// Where to create a token.
    pub token_url: &'static str,
}

async fn status_of(state: &AppState, workspace: Id) -> AppResult<Status> {
    let row: Option<(OffsetDateTime, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT i.updated_at, i.metadata, u.name FROM integrations i
         LEFT JOIN users u ON u.id = i.created_by
         WHERE i.workspace_id = $1 AND i.kind = 'hetzner'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    let usage = usage(&state.db, workspace).await?;
    Ok(match row {
        Some((at, metadata, by)) => Status {
            connected: true,
            connected_at: Some(at),
            connected_by: by,
            caps: Caps::from_metadata(&metadata),
            usage,
            token_url: super::TOKEN_HELP_URL,
        },
        None => Status {
            connected: false,
            connected_at: None,
            connected_by: None,
            caps: Caps::default(),
            usage,
            token_url: super::TOKEN_HELP_URL,
        },
    })
}

pub async fn status(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Status>> {
    Ok(Json(status_of(&state, grant.id()).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    pub token: String,
}

/// Hetzner tokens are 64 alphanumerics; accept any printable ASCII up to a bound.
fn valid_token(t: &str) -> bool {
    (16..=256).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_graphic())
}

/// Connect, or replace the token. The token must work (`GET /locations`);
/// that proves it is valid but not that it can write, which the first
/// server creation reports if not.
pub async fn connect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Connect>,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let token = req.token.trim().to_owned();
    if !valid_token(&token) {
        return Err(AppError::bad_request(
            "paste the API token from the Hetzner Cloud console",
        ));
    }
    let hetzner = Hetzner::new(&state, workspace, SecretString::from(token.clone()))?;
    match hetzner.client().locations().await {
        Ok(_) => {}
        Err(ProviderError::Unauthorized) => {
            return Err(AppError::bad_request("Hetzner rejected this token"));
        }
        Err(e) => return Err(e.into()),
    }

    let mut tx = state.db.begin().await?;
    let existing: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let id = existing.unwrap_or_default();
    let row = id.to_string();
    let sealed = state
        .keys
        .seal(&state.db, token.as_bytes(), aad(workspace, &row))
        .await?;
    sqlx::query(
        "INSERT INTO integrations (id, workspace_id, kind, secret_sealed, metadata, created_by)
         VALUES ($1, $2, 'hetzner', $3, $4, $5)
         ON CONFLICT (id) DO UPDATE SET secret_sealed = EXCLUDED.secret_sealed,
             created_by = EXCLUDED.created_by, updated_at = now()",
    )
    .bind(id)
    .bind(workspace)
    .bind(&sealed)
    .bind(serde_json::to_value(Caps::default()).map_err(anyhow::Error::from)?)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("integration.hetzner.connect", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .metadata(json!({ "replaced": existing.is_some() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdateCaps {
    pub max_servers: Option<i64>,
    pub max_monthly_cents: Option<i64>,
}

pub async fn update_caps(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateCaps>,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let mut tx = state.db.begin().await?;
    let row: Option<(Id, serde_json::Value)> = sqlx::query_as(
        "SELECT id, metadata FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, metadata)) = row else {
        return Err(AppError::Conflict("connect Hetzner first".into()));
    };
    let mut caps = Caps::from_metadata(&metadata);
    if let Some(n) = req.max_servers {
        if !(0..=Caps::MAX_SERVERS).contains(&n) {
            return Err(AppError::bad_request(format!(
                "max servers must be 0–{}",
                Caps::MAX_SERVERS
            )));
        }
        caps.max_servers = n;
    }
    if let Some(c) = req.max_monthly_cents {
        if !(0..=Caps::MAX_MONTHLY_CENTS).contains(&c) {
            return Err(AppError::bad_request("monthly limit is out of range"));
        }
        caps.max_monthly_cents = c;
    }
    sqlx::query(
        "UPDATE integrations SET metadata = metadata || $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(serde_json::to_value(caps).map_err(anyhow::Error::from)?)
    .execute(&mut *tx)
    .await?;
    Entry::new("integration.hetzner.caps", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .metadata(
            json!({ "maxServers": caps.max_servers, "maxMonthlyCents": caps.max_monthly_cents }),
        )
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

/// Refused while servers created through it exist: without the token they
/// could not be deleted from here.
pub async fn disconnect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let mut tx = state.db.begin().await?;
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provisioned_servers
         WHERE workspace_id = $1 AND provider = 'hetzner' AND provider_id IS NOT NULL",
    )
    .bind(workspace)
    .fetch_one(&mut *tx)
    .await?;
    if remaining > 0 {
        return Err(AppError::Conflict(format!(
            "delete the {remaining} server(s) created through Hetzner first"
        )));
    }
    let id: Option<Id> = sqlx::query_scalar(
        "DELETE FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner' RETURNING id",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        return Err(AppError::NotFound);
    };
    // Rows that never reached Hetzner have nothing left to clean up.
    sqlx::query("DELETE FROM provisioned_servers WHERE workspace_id = $1 AND provider = 'hetzner'")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    Entry::new("integration.hetzner.disconnect", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub locations: Vec<ProviderLocation>,
    pub server_types: Vec<ServerTypeOffer>,
}

pub(crate) async fn connected(state: &AppState, workspace: Id) -> AppResult<Hetzner> {
    provider(state, workspace)
        .await?
        .ok_or_else(|| AppError::Conflict("connect Hetzner in workspace settings first".into()))
}

/// Locations and orderable server types with gross monthly prices.
pub async fn catalog(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Catalog>> {
    let hetzner = connected(&state, grant.id()).await?;
    let (locations, server_types) =
        futures::try_join!(hetzner.locations(), hetzner.server_types())?;
    Ok(Json(Catalog {
        locations,
        server_types,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKey {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
}

pub async fn ssh_keys(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<SshKey>>> {
    let hetzner = connected(&state, grant.id()).await?;
    let keys = hetzner.client().ssh_keys().await?;
    Ok(Json(
        keys.into_iter()
            .map(|k| SshKey {
                id: k.id,
                name: k.name,
                fingerprint: k.fingerprint,
            })
            .collect(),
    ))
}
