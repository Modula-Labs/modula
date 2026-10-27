//! Servers created through a provider: list, provision, follow, delete.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::cloud_init::Bootstrap;
use super::{JOIN_TOKEN_TTL, NewServer, ServerProvider, track, valid_server_name};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::integrations::hetzner::{self, Caps, Usage};
use crate::locations::{self, Location};
use crate::state::AppState;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionedServer {
    pub id: Id,
    pub provider: String,
    pub provider_id: Option<String>,
    pub name: String,
    #[serde(skip)]
    pub location_id: String,
    #[sqlx(skip)]
    pub location: Option<&'static Location>,
    pub server_type: String,
    pub monthly_price_cents: i32,
    /// creating, booting, joining, ready, failed, deleting, resizing
    pub status: String,
    pub error: Option<String>,
    /// `provisioned` (created here) or `adopted` (linked by IP, see resize.rs).
    pub origin: String,
    pub ssh_enabled: bool,
    pub ipv4: Option<String>,
    pub node_id: Option<Id>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

macro_rules! select_servers {
    ($rest:literal) => {
        concat!(
            "SELECT id, provider, provider_id, name, location AS location_id, server_type,
                    monthly_price_cents, status, error, origin, ssh_enabled, ipv4, node_id, created_at
             FROM provisioned_servers ",
            $rest
        )
    };
}

async fn load(state: &AppState, id: Id) -> AppResult<ProvisionedServer> {
    let mut s: ProvisionedServer = sqlx::query_as(select_servers!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    s.location = locations::get(&s.location_id);
    Ok(s)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerList {
    pub servers: Vec<ProvisionedServer>,
    pub hetzner_connected: bool,
    pub caps: Caps,
    pub usage: Usage,
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<ServerList>> {
    let workspace = grant.id();
    let mut servers: Vec<ProvisionedServer> = sqlx::query_as(select_servers!(
        "WHERE workspace_id = $1 ORDER BY created_at"
    ))
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;
    for s in &mut servers {
        s.location = locations::get(&s.location_id);
    }
    let metadata: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT metadata FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(ServerList {
        servers,
        hetzner_connected: metadata.is_some(),
        caps: metadata
            .as_ref()
            .map(Caps::from_metadata)
            .unwrap_or_default(),
        usage: hetzner::usage(&state.db, workspace).await?,
    }))
}

pub async fn get(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<ProvisionedServer>> {
    Ok(Json(load(&state, grant.id()).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Provision {
    /// Catalog id, e.g. `hetzner:fsn1`.
    pub location: String,
    pub server_type: String,
    pub name: String,
    #[serde(default)]
    pub ssh_key_ids: Vec<i64>,
    /// Open port 22 in the firewall.
    #[serde(default)]
    pub ssh: bool,
}

/// Create a server at Hetzner that joins this instance. Admin + sudo; the
/// caps are checked and the row recorded in one transaction, so concurrent
/// requests cannot overshoot them.
pub async fn provision(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Provision>,
) -> AppResult<Json<ProvisionedServer>> {
    let workspace = grant.id();
    if !valid_server_name(&req.name) {
        return Err(AppError::bad_request(
            "name must be 1–63 lowercase letters, digits or hyphens",
        ));
    }
    let location = locations::get(&req.location)
        .filter(|l| l.provider == hetzner::KIND)
        .ok_or_else(|| AppError::bad_request("choose a Hetzner location"))?;
    if req.ssh_key_ids.len() > 10 || req.ssh_key_ids.iter().any(|k| *k <= 0) {
        return Err(AppError::bad_request("invalid SSH key selection"));
    }
    let provider = hetzner::api::connected(&state, workspace).await?;
    // The price comes from Hetzner, never from the request.
    let types = provider.server_types().await?;
    let price = types
        .iter()
        .find(|t| t.name == req.server_type)
        .and_then(|t| t.price_in(location.id))
        .ok_or_else(|| {
            AppError::bad_request("that server type is not available in this location")
        })?;
    let ca = state.pki().await?.fingerprint();
    // The boot script would have to carry the servers' Access service token
    // through the provider's user data; not done (yet).
    if crate::private::api::join_access(&state).await?.is_some() {
        return Err(AppError::Conflict(
            "servers can't be created from here while the dashboard is private; add one by hand (Servers → Add server)".into(),
        ));
    }

    let id = Id::new();
    let mut tx = state.db.begin().await?;
    let metadata: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::Conflict("connect Hetzner in workspace settings first".into()))?;
    let caps = Caps::from_metadata(&metadata);
    let used = hetzner::usage(&mut *tx, workspace).await?;
    if used.servers + 1 > caps.max_servers {
        return Err(AppError::Conflict(format!(
            "this workspace may have at most {} servers from Hetzner; raise the limit in settings",
            caps.max_servers
        )));
    }
    if used.monthly_cents + price > caps.max_monthly_cents {
        return Err(AppError::Conflict(format!(
            "this server would bring the monthly total to €{:.2}, over the €{:.2} limit",
            (used.monthly_cents + price) as f64 / 100.0,
            caps.max_monthly_cents as f64 / 100.0
        )));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM provisioned_servers WHERE workspace_id = $1 AND name = $2)",
    )
    .bind(workspace)
    .bind(&req.name)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(AppError::Conflict("a server with that name exists".into()));
    }
    // A join token like the ones `Add server → Manual join` mints, but
    // 30 minutes long, tied to this row and to the workspace: the server it
    // enrolls is private to it.
    let (join_id, join_token, _) = crate::nodes::create_join_token(
        &mut *tx,
        Some(actor.user_id),
        Some(workspace),
        JOIN_TOKEN_TTL,
    )
    .await?;
    sqlx::query(
        "INSERT INTO provisioned_servers
           (id, workspace_id, provider, name, location, server_type, monthly_price_cents,
            ssh_enabled, join_token_id, created_by)
         VALUES ($1, $2, 'hetzner', $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(workspace)
    .bind(&req.name)
    .bind(location.id)
    .bind(&req.server_type)
    .bind(i32::try_from(price).map_err(anyhow::Error::from)?)
    .bind(req.ssh)
    .bind(join_id)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("server.provision", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("provisioned_server", id)
        .metadata(json!({
            "provider": "hetzner",
            "name": req.name,
            "location": location.id,
            "serverType": req.server_type,
            "monthlyCents": price,
            "ssh": req.ssh,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;

    let user_data = Bootstrap::new(&state.cfg, join_token.expose_secret(), &ca, &req.name).render();
    let labels = BTreeMap::from([
        ("modula".to_owned(), "1".to_owned()),
        ("workspace".to_owned(), workspace.to_string()),
        ("modula-id".to_owned(), id.to_string()),
    ]);
    let new = NewServer {
        name: req.name.clone(),
        region: location.region.to_owned(),
        server_type: req.server_type.clone(),
        user_data,
        labels,
        ssh_key_ids: req.ssh_key_ids.clone(),
        ssh: req.ssh,
    };
    match provider.create(&new).await {
        Ok(created) => {
            sqlx::query(
                "UPDATE provisioned_servers
                 SET provider_id = $2, firewall_id = $3, provider_action_id = $4, ipv4 = $5,
                     status = 'booting', updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&created.id)
            .bind(&created.firewall_id)
            .bind(&created.action_id)
            .bind(&created.ipv4)
            .execute(&state.db)
            .await?;
        }
        Err(e) => {
            // Nothing was created: spend the token and keep the row as failed
            // (shown with the reason; deleting it frees the cap).
            burn_join_token(&state, join_id).await?;
            sqlx::query(
                "UPDATE provisioned_servers SET status = 'failed', error = $2, updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(e.to_string())
            .execute(&state.db)
            .await?;
            return Err(e.into());
        }
    }
    track::enqueue(&state, id, std::time::Duration::from_secs(3)).await?;
    Ok(Json(load(&state, id).await?))
}

async fn burn_join_token(state: &AppState, id: Id) -> AppResult<()> {
    sqlx::query(
        "UPDATE node_join_tokens SET expires_at = least(expires_at, now()) WHERE id = $1 AND used_at IS NULL",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteQuery {
    #[serde(default)]
    pub force: bool,
}

/// Delete the server at the provider and remove its node. Refused while
/// services run on it unless `force`; the user should move them first.
pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Query(q): Query<DeleteQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    let row: (Option<String>, Option<Id>, Option<Id>, String) = sqlx::query_as(
        "SELECT provider_id, join_token_id,
                COALESCE(s.node_id, (SELECT node_id FROM node_join_tokens WHERE id = s.join_token_id)),
                name
         FROM provisioned_servers s WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let (provider_id, join_token_id, node_id, name) = row;
    let (status, main): (String, bool) = sqlx::query_as(
        "SELECT s.status, coalesce(n.status <> 'revoked' AND n.info->'capabilities' ? 'dashboard', false)
         FROM provisioned_servers s LEFT JOIN nodes n ON n.id = s.node_id WHERE s.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if main {
        return Err(AppError::Conflict(format!(
            "{name} runs the dashboard; it can't be deleted from here"
        )));
    }
    if status == "resizing" {
        return Err(AppError::Conflict(format!(
            "{name} is being resized; wait until it is done"
        )));
    }

    if let Some(node) = node_id
        && !q.force
    {
        let services: Vec<String> = sqlx::query_scalar(
            "SELECT s.name || ' (' || e.name || ')' FROM service_instances i
             JOIN services s ON s.id = i.service_id
             JOIN environments e ON e.id = i.environment_id
             WHERE i.node_id = $1 ORDER BY s.name LIMIT 6",
        )
        .bind(node)
        .fetch_all(&state.db)
        .await?;
        if !services.is_empty() {
            return Err(AppError::Conflict(format!(
                "{name} runs {}: move them to another server first, or delete it anyway",
                services.join(", ")
            )));
        }
    }

    let provider = hetzner::provider(&state, workspace).await?;
    let previous: String = sqlx::query_scalar(
        "UPDATE provisioned_servers p SET status = 'deleting', updated_at = now()
         FROM provisioned_servers old WHERE p.id = $1 AND old.id = p.id RETURNING old.status",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    // A row still `creating` may have a server whose id was never recorded.
    let remote = match (&provider, provider_id) {
        (_, Some(pid)) => Some(pid),
        (Some(p), None) => p.find(&id.to_string()).await.ok().flatten().map(|s| s.id),
        (None, None) => None,
    };
    if let Some(remote) = &remote {
        let result = match &provider {
            Some(p) => p.delete(remote).await.map_err(AppError::from),
            None => Err(AppError::Conflict(
                "connect Hetzner again to delete this server".into(),
            )),
        };
        if let Err(e) = result {
            sqlx::query("UPDATE provisioned_servers SET status = $2 WHERE id = $1")
                .bind(id)
                .bind(&previous)
                .execute(&state.db)
                .await?;
            return Err(e);
        }
    }

    let mut tx = state.db.begin().await?;
    if let Some(token) = join_token_id {
        sqlx::query(
            "UPDATE node_join_tokens SET expires_at = least(expires_at, now()) WHERE id = $1 AND used_at IS NULL",
        )
        .bind(token)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(node) = node_id {
        // Same as revoking a server (api::nodes::revoke).
        crate::nodes::revoke(&mut tx, node).await?;
    }
    sqlx::query("DELETE FROM provisioned_servers WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("server.delete", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("provisioned_server", id)
        .metadata(json!({ "name": name, "providerId": remote, "node": node_id, "force": q.force }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Some(node) = node_id {
        state.hub.disconnect(node);
    }
    Ok(Json(json!({ "ok": true })))
}
