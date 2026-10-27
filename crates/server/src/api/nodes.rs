//! Servers (nodes). Instance admins only: nodes are shared by every workspace
//! on a self-hosted instance.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::locations::{self, Location};
use crate::metrics::{DiskSeries, NodePoint, Range};
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
    /// The IPv4 it reaches the internet from, as it last reported.
    pub public_ip: Option<String>,
}

macro_rules! select_nodes {
    ($rest:literal) => {
        concat!(
            "SELECT id, name, status, node_version, runtime, info, desired_generation, applied_generation,
                    last_seen_at, cert_expires_at, created_at, location AS location_id, location_pinned,
                    public_ip
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

#[derive(Deserialize)]
pub struct MetricsParams {
    #[serde(default)]
    range: Range,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetrics {
    range: Range,
    step_secs: i64,
    points: Vec<NodePoint>,
    /// Disk buckets are longer (the node reports disk every 5 minutes).
    disk_step_secs: i64,
    disk: Vec<DiskSeries>,
}

/// `GET /api/nodes/{id}/metrics?range=1h|6h|24h|7d`: the server's own CPU,
/// memory, load and network, and its filesystems' usage.
pub async fn metrics(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<MetricsParams>,
) -> AppResult<Json<NodeMetrics>> {
    let id = grant.id();
    let step_secs = p.range.span_and_step().1;
    Ok(Json(NodeMetrics {
        range: p.range,
        step_secs,
        points: crate::metrics::node_series(&state.db, id, p.range).await?,
        disk_step_secs: step_secs.max(crate::metrics::DISK_MIN_STEP_SECS),
        disk: crate::metrics::disk_series(&state.db, id, p.range).await?,
    }))
}

/// Every location a server can be placed in, for the picker.
/// The private network: every server in it and, per pair, the path the
/// first takes to the second and what it last measured.
pub async fn network(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let rows: Vec<(Id, String, Option<i16>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, name, mesh_index, mesh_status FROM nodes
         WHERE status <> 'revoked' ORDER BY created_at",
    )
    .fetch_all(&state.db)
    .await?;
    let mut nodes = Vec::with_capacity(rows.len());
    let mut links = Vec::new();
    for (id, name, index, status) in &rows {
        nodes.push(json!({
            "id": id,
            "name": name,
            "subnet": index.map(crate::mesh::subnet),
            "connected": state.hub.is_connected(*id),
            "reportedAt": status.get("at"),
        }));
        let Some((me, peers)) = crate::mesh::nodes(&state.db, *id).await? else {
            continue;
        };
        let reported = status.get("peers").and_then(|p| p.as_array());
        let hub_present = me.hub || peers.iter().any(|p| p.hub);
        if let Some(n) = nodes.last_mut() {
            n["relayed"] = json!(me.relay);
        }
        for p in peers {
            let relayed = crate::mesh::path(&me, &p, hub_present) == crate::mesh::Path::Relay;
            let (endpoint, private) = crate::mesh::endpoint(&me, &p);
            let seen = reported.and_then(|r| {
                r.iter()
                    .find(|x| x.get("nodeId").and_then(|v| v.as_str()) == Some(p.id.as_str()))
            });
            let latency_us = seen
                .and_then(|s| s.get("latencyUs"))
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0);
            links.push(json!({
                "from": id,
                "to": p.id,
                "path": match (relayed, endpoint.is_empty(), private) {
                    (true, _, _) => "relay",
                    (false, true, _) => "none",
                    (false, false, true) => "private",
                    (false, false, false) => "public",
                },
                "endpoint": (!relayed && !endpoint.is_empty()).then_some(endpoint),
                "latencyMs": latency_us.map(|us| us as f64 / 1000.0),
                "lastHandshake": seen
                    .and_then(|s| s.get("lastHandshake"))
                    .and_then(|v| v.as_i64())
                    .filter(|v| *v > 0),
            }));
        }
    }
    Ok(Json(json!({ "nodes": nodes, "links": links })))
}

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
    // Instance admin: the server is shared by every workspace.
    let (_, token, expires_at) =
        crate::nodes::create_join_token(&state.db, Some(actor.user_id), None, JOIN_TOKEN_TTL)
            .await?;
    let fingerprint = state.pki().await?.fingerprint();
    let server = state.cfg.node_url.as_str().trim_end_matches('/').to_owned();
    Entry::new("node.join_token.create", &meta)
        .actor(&actor)
        .metadata(json!({ "expiresAt": expires_at.unix_timestamp() }))
        .record(&state.db)
        .await?;
    let token = token.expose_secret().to_owned();
    let mut command = crate::nodes::join_command(&state.cfg, &token, &fingerprint);
    let mut node_command = crate::nodes::node_join_command(&state.cfg, &token, &fingerprint);
    // Private mode: new servers reach the node endpoint through the tunnel.
    if let Some(access) = crate::private::api::join_access(&state).await? {
        command = access.apply(&command, &server);
        node_command = access.apply(&node_command, &server);
    }
    Ok(Json(JoinToken {
        command,
        node_command,
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
    // Already revoked: nothing to do (and nothing to audit).
    if crate::nodes::revoke(&mut tx, id).await? {
        Entry::new("node.revoke", &meta)
            .actor(&actor)
            .target("node", id)
            .record(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    state.hub.disconnect(id);
    Ok(Json(json!({ "ok": true })))
}
