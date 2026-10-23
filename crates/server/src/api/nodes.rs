//! Servers (nodes). Instance admins only: nodes are shared by every workspace
//! on a self-hosted instance.

use axum::Json;
use axum::extract::State;
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::{self, LiveFilter};
use crate::error::AppResult;
use crate::http::meta::RequestMeta;
use crate::state::AppState;

const JOIN_TOKEN_TTL: time::Duration = time::Duration::hours(1);

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: Uuid,
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
}

pub async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<Node>>> {
    let mut nodes: Vec<Node> = sqlx::query_as(
        "SELECT id, name, status, node_version, runtime, info, desired_generation, applied_generation,
                last_seen_at, cert_expires_at, created_at
         FROM nodes WHERE status <> 'revoked' ORDER BY created_at",
    )
    .fetch_all(&state.db)
    .await?;
    for n in &mut nodes {
        n.connected = state.hub.is_connected(n.id);
    }
    Ok(Json(nodes))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinToken {
    pub token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub server: String,
    pub fingerprint: String,
    /// Ready-to-paste command for the new server.
    pub command: String,
}

pub async fn create_join_token(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
) -> AppResult<Json<JoinToken>> {
    let (token, expires_at) =
        crate::nodes::grpc::create_join_token(&state, actor.user_id, JOIN_TOKEN_TTL).await?;
    let fingerprint = state.pki().await?.fingerprint();
    let server = state.cfg.node_url.as_str().trim_end_matches('/').to_owned();
    Entry::new("node.join_token.create", &meta)
        .actor(&actor)
        .metadata(json!({ "expiresAt": expires_at.unix_timestamp() }))
        .record(&state.db)
        .await?;
    let token = token.expose_secret().to_owned();
    Ok(Json(JoinToken {
        command: format!("modula-node join --server {server} --token {token} --ca {fingerprint}"),
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
