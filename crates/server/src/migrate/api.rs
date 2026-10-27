//! Server migrations and service moves over HTTP.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::moves::{self, NewMove, Table};
use super::server;
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::{Grant, Resource, ResourceKind, Role, Scope};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::locations::{self, Location};
use crate::provision::api::Provision;
use crate::state::AppState;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct MoveView {
    pub id: Id,
    pub migration_id: Option<Id>,
    pub service_instance_id: Id,
    pub service: String,
    pub environment: String,
    pub project: String,
    /// `primary`: the service itself moves; `replicas`: only its replicas on
    /// the old server.
    pub kind: String,
    pub from_node_id: Option<Id>,
    pub from_name: Option<String>,
    pub to_node_id: Option<Id>,
    pub to_name: Option<String>,
    /// pending, running, failed, aborting, aborted, done
    pub status: String,
    /// stop, stopping, copy, deploy, wait, done
    pub phase: String,
    pub deployment_id: Option<Id>,
    pub copied_bytes: i64,
    pub steps: serde_json::Value,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

macro_rules! select_moves {
    ($rest:literal) => {
        concat!(
            "SELECT m.id, m.migration_id, m.service_instance_id, s.name AS service, e.name AS environment,
                    p.name AS project, m.kind, m.from_node_id, f.name AS from_name, m.to_node_id,
                    t.name AS to_name, m.status, m.phase, m.deployment_id, m.copied_bytes, m.steps,
                    m.error, m.created_at, m.finished_at
             FROM instance_moves m
             JOIN service_instances i ON i.id = m.service_instance_id
             JOIN services s ON s.id = i.service_id
             JOIN environments e ON e.id = i.environment_id
             JOIN projects p ON p.id = s.project_id
             LEFT JOIN nodes f ON f.id = m.from_node_id
             LEFT JOIN nodes t ON t.id = m.to_node_id ",
            $rest
        )
    };
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct MigrationView {
    pub id: Id,
    pub workspace_id: Id,
    pub source_node_id: Option<Id>,
    pub source_server_id: Option<Id>,
    pub source_name: String,
    #[serde(skip)]
    pub target_location_id: String,
    #[sqlx(skip)]
    pub target_location: Option<&'static Location>,
    pub target_server_type: String,
    pub target_server_id: Option<Id>,
    pub target_name: Option<String>,
    pub target_node_id: Option<Id>,
    /// running, failed, aborting, aborted, done
    pub status: String,
    /// create, join, move, promote, finish
    pub step: String,
    /// The old server runs the control plane, which moves too.
    pub main: bool,
    /// Step promote: bundle, install, verify, switch.
    pub promote_phase: Option<String>,
    /// What the new server last reported while installing.
    pub promote_detail: Option<String>,
    /// Where the dashboard is after a main server migration.
    pub new_public_url: Option<String>,
    pub steps: serde_json::Value,
    pub error: Option<String>,
    /// The old server is `pending_deletion` (the user may delete it now).
    pub source_pending_deletion: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    #[sqlx(skip)]
    pub moves: Vec<MoveView>,
    /// TCP proxies' addresses on the new server, once moved.
    #[sqlx(skip)]
    pub tcp_proxies: Vec<ProxyAddress>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ProxyAddress {
    pub service: String,
    pub host: Option<String>,
    pub public_port: i32,
    pub target_port: i32,
}

macro_rules! select_migrations {
    ($rest:literal) => {
        concat!(
            "SELECT m.id, m.workspace_id, m.source_node_id, m.source_server_id, m.source_name,
                    m.target_location AS target_location_id, m.target_server_type, m.target_server_id,
                    t.name AS target_name, m.target_node_id, m.status, m.step, m.main,
                    m.promote_phase, m.promote_detail, m.new_public_url, m.steps, m.error,
                    coalesce(o.status = 'pending_deletion', false) AS source_pending_deletion,
                    m.created_at, m.finished_at
             FROM server_migrations m
             LEFT JOIN provisioned_servers t ON t.id = m.target_server_id
             LEFT JOIN provisioned_servers o ON o.id = m.source_server_id ",
            $rest
        )
    };
}

async fn load_migration(state: &AppState, id: Id) -> AppResult<MigrationView> {
    let mut m: MigrationView = sqlx::query_as(select_migrations!("WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    fill(state, &mut m).await?;
    Ok(m)
}

async fn fill(state: &AppState, m: &mut MigrationView) -> AppResult<()> {
    m.target_location = locations::get(&m.target_location_id);
    m.moves = sqlx::query_as(select_moves!(
        "WHERE m.migration_id = $1 ORDER BY m.created_at, m.id"
    ))
    .bind(m.id)
    .fetch_all(&state.db)
    .await?;
    m.tcp_proxies = match m.target_node_id {
        Some(node) => {
            sqlx::query_as(
                "SELECT s.name AS service, n.public_ip AS host, t.public_port, t.target_port
                 FROM tcp_proxies t
                 JOIN service_instances i ON i.id = t.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN nodes n ON n.id = t.node_id
                 JOIN instance_moves mv ON mv.service_instance_id = i.id AND mv.migration_id = $2
                 WHERE t.node_id = $1 ORDER BY s.name",
            )
            .bind(node)
            .bind(m.id)
            .fetch_all(&state.db)
            .await?
        }
        None => Vec::new(),
    };
    Ok(())
}

async fn load_move(state: &AppState, id: Id) -> AppResult<MoveView> {
    sqlx::query_as(select_moves!("WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------- server migrations

#[derive(sqlx::FromRow)]
struct Source {
    name: String,
    main: bool,
    server_id: Option<Id>,
    server_workspace: Option<Id>,
    server_status: Option<String>,
    location: Option<String>,
    ssh_enabled: Option<bool>,
}

async fn source(state: &AppState, node: Id) -> AppResult<Source> {
    sqlx::query_as(
        "SELECT n.name, coalesce(n.info->'capabilities' ? 'dashboard', false) AS main,
                p.id AS server_id, p.workspace_id AS server_workspace, p.status AS server_status,
                coalesce(p.location, n.location) AS location, p.ssh_enabled
         FROM nodes n
         LEFT JOIN provisioned_servers p ON p.node_id = n.id AND p.status NOT IN ('failed', 'deleting')
         WHERE n.id = $1 AND n.status <> 'revoked'",
    )
    .bind(node)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Why the main server can't be migrated now, if it can't.
fn main_blocker(state: &AppState) -> Option<String> {
    state.private.is_on().then(|| {
        "the dashboard is private (Cloudflare Tunnel): turn private mode off before moving the main server, and on again after".to_owned()
    })
}

/// The main server is usually not created from the dashboard: link it to
/// its provider server by IP first (as resizing does).
async fn link_main(
    state: &AppState,
    node: Id,
    src: &mut Source,
    actor: Id,
) -> AppResult<Option<String>> {
    if src.server_id.is_some() {
        return Ok(None);
    }
    match crate::provision::resize::link_node(state, node, actor).await? {
        Ok(_) => {
            *src = source(state, node).await?;
            Ok(None)
        }
        Err(why) => Ok(Some(why)),
    }
}

/// Instance admins act on servers; creating one spends the workspace's
/// provider account, so they must also be an admin there.
async fn require_workspace_admin(state: &AppState, actor: &Actor, workspace: Id) -> AppResult<()> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace)
    .bind(actor.user_id)
    .fetch_optional(&state.db)
    .await?;
    if role.as_deref().and_then(Role::parse) == Some(Role::Admin)
        && actor.can_reach_workspace(workspace)
    {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewVolume {
    pub id: Id,
    pub mount_path: String,
    /// Last measured on the server; None before the first measurement.
    pub used_bytes: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PreviewService {
    pub instance_id: Id,
    pub service: String,
    pub environment: String,
    pub project: String,
    /// It runs only replicas on this server.
    pub replica_only: bool,
    #[sqlx(json)]
    pub volumes: Vec<PreviewVolume>,
    /// Its TCP proxy's current address (it changes with the move).
    pub tcp_address: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub node_id: Id,
    pub name: String,
    /// The main server: the control plane moves too.
    pub main: bool,
    /// Why the main server can't be migrated now.
    pub main_blocker: Option<String>,
    /// The dashboard's address now.
    pub current_url: String,
    /// The dashboard is reached by IP: its address changes with the move.
    pub ip_based: bool,
    /// Names whose DNS must point at the new server (main server).
    pub dashboard_hosts: Vec<String>,
    /// The provisioned server and its workspace (whose catalog to pick from);
    /// None = not linked to a provider server, so it can't be migrated.
    pub server_id: Option<Id>,
    pub workspace_id: Option<Id>,
    pub location: Option<&'static Location>,
    pub services: Vec<PreviewService>,
    pub total_volume_bytes: i64,
    pub active_migration_id: Option<Id>,
}

/// What a migration of this server would move: services, volume sizes and
/// TCP proxy addresses, for the confirmation dialog.
pub async fn preview(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
) -> AppResult<Json<Preview>> {
    let node = grant.id();
    let mut src = source(&state, node).await?;
    let main_blocker = if src.main {
        match main_blocker(&state) {
            Some(b) => Some(b),
            None => link_main(&state, node, &mut src, actor.user_id).await?,
        }
    } else {
        None
    };
    let services: Vec<PreviewService> = sqlx::query_as(
        "SELECT i.id AS instance_id, s.name AS service, e.name AS environment, p.name AS project,
                i.node_id IS DISTINCT FROM $1 AS replica_only,
                coalesce((SELECT json_agg(json_build_object('id', v.id, 'mountPath', v.mount_path,
                                                            'usedBytes', u.used_bytes) ORDER BY v.mount_path)
                          FROM volumes v LEFT JOIN volume_usage u ON u.volume_id = v.id
                          WHERE v.service_instance_id = i.id), '[]'::json) AS volumes,
                (SELECT n.public_ip || ':' || t.public_port FROM tcp_proxies t JOIN nodes n ON n.id = t.node_id
                 WHERE t.service_instance_id = i.id) AS tcp_address
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         JOIN projects p ON p.id = s.project_id
         WHERE i.node_id = $1
            OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                       WHERE d.service_instance_id = i.id AND rp.node_id = $1
                         AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed'))
         ORDER BY p.name, s.name, e.name",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    let total = services
        .iter()
        .flat_map(|s| &s.volumes)
        .filter_map(|v| v.used_bytes)
        .sum();
    let active: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM server_migrations WHERE source_node_id = $1
         AND status IN ('running', 'failed', 'aborting')",
    )
    .bind(node)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(Preview {
        node_id: node,
        name: src.name,
        main: src.main,
        main_blocker,
        current_url: state.public_url().as_str().trim_end_matches('/').to_owned(),
        ip_based: src.main && super::promote::ip_based(&state),
        dashboard_hosts: if src.main {
            super::promote::dashboard_hosts(&state)
        } else {
            Vec::new()
        },
        server_id: src.server_id,
        workspace_id: src.server_workspace,
        location: src.location.as_deref().and_then(locations::get),
        services,
        total_volume_bytes: total,
        active_migration_id: active,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Migrate {
    /// Catalog id of the new server's location, e.g. `hetzner:hel1`.
    pub location: String,
    pub server_type: String,
}

/// The new server's name: the old one's with a short random suffix.
fn new_name(old: &str) -> String {
    let base: String = old
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(56)
        .collect();
    let base = base.trim_matches('-');
    // From the id's random tail: its head is a timestamp, the same for
    // every name made within half an hour (so a retry would collide).
    let id = Id::new().to_string();
    let suffix = &id[id.len() - 5..];
    if base.is_empty() {
        format!("server-{suffix}")
    } else {
        format!("{base}-{suffix}")
    }
}

/// Migrate a worker server: create the new one now (the provisioning path,
/// so the workspace's caps apply) and move everything in the background.
pub async fn migrate(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Migrate>,
) -> AppResult<Json<MigrationView>> {
    let node = grant.id();
    let mut src = source(&state, node).await?;
    if src.main {
        if let Some(why) = main_blocker(&state) {
            return Err(AppError::Conflict(why));
        }
        if let Some(why) = link_main(&state, node, &mut src, actor.user_id).await? {
            return Err(AppError::Conflict(format!(
                "the main server can't be migrated from here: {why}"
            )));
        }
        let other_main: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM server_migrations WHERE main AND status IN ('running', 'failed', 'aborting'))",
        )
        .fetch_one(&state.db)
        .await?;
        if other_main {
            return Err(AppError::Conflict(
                "the main server is already being migrated".into(),
            ));
        }
    }
    let (Some(server_id), Some(workspace)) = (src.server_id, src.server_workspace) else {
        return Err(AppError::Conflict(
            "only servers linked to Hetzner can be migrated for now; this one was added by hand"
                .into(),
        ));
    };
    if src.server_status.as_deref() != Some("ready") {
        return Err(AppError::Conflict(
            "the server must be ready (not being created, resized or deleted) to migrate it".into(),
        ));
    }
    require_workspace_admin(&state, &actor, workspace).await?;
    let name = new_name(&src.name);
    let id = Id::new();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_server_id, source_name,
                                        target_location, target_server_type, created_by, steps, main)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '[]', $9)",
    )
    .bind(id)
    .bind(workspace)
    .bind(node)
    .bind(server_id)
    .bind(&src.name)
    .bind(&req.location)
    .bind(&req.server_type)
    .bind(actor.user_id)
    .bind(src.main)
    .execute(&mut *tx)
    .await;
    match inserted {
        Err(sqlx::Error::Database(d)) if d.constraint() == Some("server_migrations_source_key") => {
            return Err(AppError::Conflict(
                "this server is already being migrated; abort that migration first".into(),
            ));
        }
        r => {
            r?;
        }
    }
    let city = locations::get(&req.location).map_or(req.location.as_str(), |l| l.city);
    moves::begin(
        &mut *tx,
        Table::Migrations,
        id,
        &format!("Creating {name} ({}) in {city}", req.server_type),
    )
    .await?;
    tx.commit().await?;

    // Same checks and caps as Servers → Add server.
    let created = crate::provision::api::provision(
        State(state.clone()),
        actor.clone(),
        Grant {
            scope: Scope::Workspace {
                id: workspace,
                role: Role::Admin,
            },
            resource: Resource {
                kind: ResourceKind::Workspace,
                id: workspace,
            },
        },
        meta.clone(),
        Json(Provision {
            location: req.location.clone(),
            server_type: req.server_type.clone(),
            name: name.clone(),
            ssh_key_ids: Vec::new(),
            ssh: src.ssh_enabled.unwrap_or(false),
        }),
    )
    .await;
    let created = match created {
        Ok(Json(s)) => s,
        Err(e) => {
            // Nothing moved: record why, and free the server for another try.
            let why = moves::error_text(&e);
            moves::end(&state.db, Table::Migrations, id, "failed", Some(&why)).await?;
            sqlx::query(
                "UPDATE server_migrations SET status = 'aborted', error = $2, finished_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(&why)
            .execute(&state.db)
            .await?;
            return Err(e);
        }
    };
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE server_migrations SET target_server_id = $2, step = 'join', updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(created.id)
    .execute(&mut *tx)
    .await?;
    moves::end(&mut *tx, Table::Migrations, id, "done", None).await?;
    moves::begin(
        &mut *tx,
        Table::Migrations,
        id,
        &format!("Waiting for {name} to boot and join"),
    )
    .await?;
    Entry::new("server.migrate", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("server_migration", id)
        .metadata(json!({
            "node": node,
            "from": src.name,
            "fromServer": server_id,
            "toServer": created.id,
            "name": name,
            "location": req.location,
            "serverType": req.server_type,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    server::enqueue(&state, id, std::time::Duration::from_secs(5)).await?;
    Ok(Json(load_migration(&state, id).await?))
}

/// A server's migrations, newest first.
pub async fn list(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<MigrationView>>> {
    let mut all: Vec<MigrationView> = sqlx::query_as(select_migrations!(
        "WHERE m.source_node_id = $1 OR m.target_node_id = $1 ORDER BY m.created_at DESC LIMIT 20"
    ))
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    for m in &mut all {
        fill(&state, m).await?;
    }
    Ok(Json(all))
}

pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<MigrationView>> {
    Ok(Json(load_migration(&state, grant.id()).await?))
}

/// Stop a running or failed migration and put every moved service back on
/// the old server. The new server stays (delete it from Servers).
pub async fn abort(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<MigrationView>> {
    let id = grant.id();
    if !server::abort(&state, id).await? {
        return Err(AppError::Conflict(
            "only a running or failed migration can be aborted (not once the hand-over to the new main server began)".into(),
        ));
    }
    Entry::new("server.migrate.abort", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("server_migration", id)
        .record(&state.db)
        .await?;
    Ok(Json(load_migration(&state, id).await?))
}

// ---------------------------------------------------------------- one service

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MoveInstance {
    pub node_id: Id,
}

/// Move a service (its volumes' data included) to another server. A service
/// with a volume is down from when it stops on the old server until it is
/// healthy on the new one; one without keeps serving meanwhile.
pub async fn move_instance(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<MoveInstance>,
) -> AppResult<Json<MoveView>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let target: Option<(String, Option<Id>, bool)> = sqlx::query_as(
        "SELECT name, workspace_id,
                EXISTS (SELECT 1 FROM server_migrations m WHERE m.source_node_id = n.id
                        AND m.status IN ('running', 'failed', 'aborting', 'done'))
         FROM nodes n WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(req.node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((to_name, _, draining)) =
        target.filter(|(_, ws, _)| ws.is_none_or(|w| w == workspace))
    else {
        return Err(AppError::bad_request("no such server"));
    };
    if draining {
        return Err(AppError::Conflict(format!(
            "{to_name} is being migrated away; choose another server"
        )));
    }
    let (from, has_volume): (Option<Id>, bool) = sqlx::query_as(
        "SELECT node_id, EXISTS (SELECT 1 FROM volumes v WHERE v.service_instance_id = i.id)
         FROM service_instances i WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    if from == Some(req.node_id) {
        return Err(AppError::Conflict(format!("it already runs on {to_name}")));
    }
    if has_volume && !state.hub.is_connected(req.node_id) {
        return Err(AppError::Conflict(format!(
            "{to_name} is offline; its data can only be copied to an online server"
        )));
    }
    if let Some(f) = from
        && has_volume
        && !state.hub.is_connected(f)
    {
        return Err(AppError::Conflict(
            "the service's current server is offline, so its data can't be copied".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let id = moves::create(
        &mut *tx,
        NewMove {
            workspace,
            instance,
            migration: None,
            kind: "primary",
            from,
            to: req.node_id,
            created_by: Some(actor.user_id),
            status: "running",
        },
    )
    .await?;
    Entry::new("service_instance.move", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({ "from": from, "to": req.node_id, "move": id, "withData": has_volume }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    moves::enqueue(&state, id, std::time::Duration::ZERO).await?;
    Ok(Json(load_move(&state, id).await?))
}

pub async fn get_move(State(state): State<AppState>, grant: Grant) -> AppResult<Json<MoveView>> {
    Ok(Json(load_move(&state, grant.id()).await?))
}

/// Stop a move and put the service back on its old server. Moves that are
/// part of a server migration are aborted with the migration.
pub async fn abort_move(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<MoveView>> {
    let id = grant.id();
    let part_of: Option<Option<Id>> =
        sqlx::query_scalar("SELECT migration_id FROM instance_moves WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    if part_of.flatten().is_some() {
        return Err(AppError::Conflict(
            "this move is part of a server migration; abort the migration instead".into(),
        ));
    }
    if !moves::restore(&state, id, false).await? {
        return Err(AppError::Conflict(
            "only a running or failed move can be aborted".into(),
        ));
    }
    Entry::new("service_instance.move.abort", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("instance_move", id)
        .record(&state.db)
        .await?;
    Ok(Json(load_move(&state, id).await?))
}

#[cfg(test)]
mod tests {
    use super::new_name;

    #[test]
    fn new_names_are_valid_server_names() {
        for old in ["worker-1", "a", "x".repeat(63).as_str(), "-weird-"] {
            let n = new_name(old);
            assert!(crate::provision::valid_server_name(&n), "{n}");
            assert_ne!(n, old);
        }
    }

    #[test]
    fn new_names_differ_between_attempts() {
        let names: std::collections::HashSet<String> = (0..20).map(|_| new_name("w1")).collect();
        assert!(names.len() > 15, "{names:?}");
    }
}
