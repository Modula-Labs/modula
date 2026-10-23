//! Servers (nodes). Instance admins only: nodes are shared by every workspace
//! on a self-hosted instance.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::locations::{self, Location};
use crate::state::AppState;

const JOIN_TOKEN_TTL: time::Duration = time::Duration::hours(1);

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: Id,
    pub name: String,
    pub status: String,
    pub node_version: Option<String>,
    pub runtime: Option<String>,
    pub info: serde_json::Value,
    pub desired_generation: i64,
    pub applied_generation: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub cert_expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[sqlx(skip)]
    pub connected: bool,
    #[serde(skip)]
    pub location_id: Option<String>,
    #[sqlx(skip)]
    pub location: Option<&'static Location>,
    /// Set by an admin rather than detected.
    pub location_pinned: bool,
}

macro_rules! select_nodes {
    ($rest:literal) => {
        concat!(
            "SELECT id, name, status, node_version, runtime, info, desired_generation, applied_generation,
                    last_seen_at, cert_expires_at, created_at, location AS location_id, location_pinned
             FROM nodes ",
            $rest
        )
    };
}

pub async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<Node>>> {
    let mut nodes: Vec<Node> = sqlx::query_as(select_nodes!(
        "WHERE status <> 'revoked' ORDER BY created_at"
    ))
    .fetch_all(&state.db)
    .await?;
    for n in &mut nodes {
        n.connected = state.hub.is_connected(n.id);
        n.location = n.location_id.as_deref().and_then(locations::get);
    }
    Ok(Json(nodes))
}

/// Every location a server can be placed in, for the picker.
pub async fn locations() -> Json<&'static [Location]> {
    Json(locations::CATALOG)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateNode {
    /// A catalog id to pin, or null to go back to detection.
    pub location: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateNode>,
) -> AppResult<Json<Node>> {
    let id = grant.id();
    let location = match req.location.as_deref() {
        Some(l) => Some(
            locations::get(l)
                .ok_or_else(|| AppError::bad_request("unknown location"))?
                .id,
        ),
        None => None,
    };
    let mut tx = state.db.begin().await?;
    // Unpinning forgets the old value; the next connect detects it again.
    sqlx::query("UPDATE nodes SET location = $2, location_pinned = $3 WHERE id = $1")
        .bind(id)
        .bind(location)
        .bind(location.is_some())
        .execute(&mut *tx)
        .await?;
    Entry::new("node.update", &meta)
        .actor(&actor)
        .target("node", id)
        .metadata(json!({ "location": location }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut node: Node = sqlx::query_as(select_nodes!("WHERE id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    node.connected = state.hub.is_connected(node.id);
    node.location = node.location_id.as_deref().and_then(locations::get);
    Ok(Json(node))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinToken {
    pub token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub server: String,
    pub fingerprint: String,
    /// Ready-to-paste command for a fresh server (installs, then joins).
    pub command: String,
    /// The join step alone, for a server that already runs modula-node.
    pub node_command: String,
}

pub async fn create_join_token(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
) -> AppResult<Json<JoinToken>> {
    let (token, expires_at) =
        crate::nodes::create_join_token(&state.db, Some(actor.user_id), JOIN_TOKEN_TTL).await?;
    let fingerprint = state.pki().await?.fingerprint();
    let server = state.cfg.node_url.as_str().trim_end_matches('/').to_owned();
    Entry::new("node.join_token.create", &meta)
        .actor(&actor)
        .metadata(json!({ "expiresAt": expires_at.unix_timestamp() }))
        .record(&state.db)
        .await?;
    let token = token.expose_secret().to_owned();
    Ok(Json(JoinToken {
        command: crate::nodes::join_command(&state.cfg, &token, &fingerprint),
        node_command: crate::nodes::node_join_command(&state.cfg, &token, &fingerprint),
        token,
        expires_at,
        server,
        fingerprint,
    }))
}

pub async fn revoke(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE nodes SET status = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    deploy::remove_live(&mut tx, LiveFilter::Node, id).await?;
    sqlx::query("UPDATE service_instances SET node_id = NULL WHERE node_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("node.revoke", &meta)
        .actor(&actor)
        .target("node", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.disconnect(id);
    Ok(Json(json!({ "ok": true })))
}
