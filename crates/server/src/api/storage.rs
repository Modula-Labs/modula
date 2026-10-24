//! Database templates, volumes and TCP proxies: what makes a service a
//! stateful, reachable database.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::Deserialize;
use serde_json::json;

use crate::api::services::{Instance, load_instance};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::{self, placement};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::templates::{self, Template};

const TCP_PORTS: std::ops::RangeInclusive<i32> = 20000..=29999;

pub async fn templates() -> Json<&'static [Template]> {
    Json(templates::CATALOG)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateVolume {
    pub mount_path: String,
}

fn validate_mount_path(p: &str) -> AppResult<()> {
    let ok = p.starts_with('/')
        && p.len() <= 255
        && !p.contains("..")
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    // Mounting over system directories breaks the container (or worse).
    let reserved = [
        "/", "/proc", "/sys", "/dev", "/etc", "/bin", "/sbin", "/lib", "/usr",
    ];
    if !ok || reserved.contains(&p.trim_end_matches('/')) || p == "/" {
        return Err(AppError::bad_request(
            "mount path must be an absolute path like /data (letters, digits, . _ - /)",
        ));
    }
    Ok(())
}

/// Attach a volume. Takes effect on the next deploy.
pub async fn create_volume(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateVolume>,
) -> AppResult<Json<Instance>> {
    let instance = grant.id();
    validate_mount_path(&req.mount_path)?;
    let serverless: bool =
        sqlx::query_scalar("SELECT config ? 'serverless' FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    if serverless {
        return Err(AppError::Conflict(
            "a serverless service can't have a volume: turn serverless off first".into(),
        ));
    }
    let id = Id::new();
    let r = sqlx::query(
        "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, $3)
         ON CONFLICT (service_instance_id) DO NOTHING",
    )
    .bind(id)
    .bind(instance)
    .bind(req.mount_path.trim_end_matches('/'))
    .execute(&state.db)
    .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::Conflict(
            "this service already has a volume".into(),
        ));
    }
    Entry::new("volume.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("volume", id)
        .metadata(json!({ "serviceInstance": instance, "mountPath": req.mount_path }))
        .record(&state.db)
        .await?;
    load_instance(&state, instance).await.map(Json)
}

/// Detach a volume and delete its data on the node. The running deployment
/// keeps its mount until the next deploy.
pub async fn delete_volume(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let (instance, node): (Id, Option<Id>) = sqlx::query_as(
        "DELETE FROM volumes v USING service_instances i
         WHERE v.id = $1 AND i.id = v.service_instance_id RETURNING i.id, i.node_id",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(n) = node {
        sqlx::query("INSERT INTO volume_removals (volume_id, node_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(n)
            .execute(&mut *tx)
            .await?;
        deploy::bump(&mut *tx, n).await?;
    }
    Entry::new("volume.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("volume", id)
        .metadata(json!({ "serviceInstance": instance }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Some(n) = node {
        state.hub.kick_all(&[n]);
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateTcpProxy {
    pub target_port: u16,
}

/// Expose a service's port on its server's public address (a port from
/// 20000–29999), e.g. for database clients.
pub async fn create_tcp_proxy(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateTcpProxy>,
) -> AppResult<Json<Instance>> {
    let instance = grant.id();
    if req.target_port == 0 {
        return Err(AppError::bad_request("target port must be 1–65535"));
    }
    let mut tx = state.db.begin().await?;
    // The proxy lives on the node that runs (or will run) the service.
    let node = placement::choose(&mut tx, instance).await?;
    // Serialize allocations per node so two requests can't pick one port.
    sqlx::query("SELECT 1 FROM nodes WHERE id = $1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let used: Vec<i32> =
        sqlx::query_scalar("SELECT public_port FROM tcp_proxies WHERE node_id = $1")
            .bind(node)
            .fetch_all(&mut *tx)
            .await?;
    let port = TCP_PORTS
        .clone()
        .find(|p| !used.contains(p))
        .ok_or_else(|| AppError::Conflict("this server has no free TCP proxy ports".into()))?;
    let id = Id::new();
    let r = sqlx::query(
        "INSERT INTO tcp_proxies (id, service_instance_id, node_id, target_port, public_port)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (service_instance_id) DO NOTHING",
    )
    .bind(id)
    .bind(instance)
    .bind(node)
    .bind(i32::from(req.target_port))
    .bind(port)
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::Conflict(
            "this service already has a TCP proxy".into(),
        ));
    }
    deploy::bump(&mut *tx, node).await?;
    Entry::new("tcp_proxy.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("tcp_proxy", id)
        .metadata(json!({ "serviceInstance": instance, "targetPort": req.target_port, "publicPort": port }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&[node]);
    load_instance(&state, instance).await.map(Json)
}

pub async fn delete_tcp_proxy(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let (instance, node): (Id, Id) = sqlx::query_as(
        "DELETE FROM tcp_proxies WHERE id = $1 RETURNING service_instance_id, node_id",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    deploy::bump(&mut *tx, node).await?;
    Entry::new("tcp_proxy.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("tcp_proxy", id)
        .metadata(json!({ "serviceInstance": instance }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&[node]);
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_paths() {
        for ok in ["/data", "/var/lib/postgresql/data", "/app/uploads_2"] {
            validate_mount_path(ok).unwrap();
        }
        for bad in [
            "", "data", "/", "/etc", "/proc/", "/a/../b", "/a b", "/x;rm",
        ] {
            assert!(validate_mount_path(bad).is_err(), "{bad}");
        }
    }
}
