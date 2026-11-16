//! Server migrations and service moves over HTTP.

use axum::Json;
use axum::extract::{Query, State};
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
    pub target_location_id: Option<String>,
    #[sqlx(skip)]
    pub target_location: Option<&'static Location>,
    /// Unknown for an existing target not at a provider.
    pub target_server_type: Option<String>,
    /// Set only when this migration created the target server.
    pub target_server_id: Option<Id>,
    pub target_name: Option<String>,
    pub target_node_id: Option<Id>,
    /// The target server already existed: aborting leaves it alone.
    pub target_existing: bool,
    /// running, failed, aborting, aborted, done
    pub status: String,
    /// create, join, move, promote, dns, finish
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
    /// DNS records repointed so far (step dns).
    pub dns_changes: serde_json::Value,
    /// Services still holding a moved TCP proxy's old address: they need a
    /// redeploy.
    pub stale_tcp: serde_json::Value,
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
                    coalesce(t.name, tn.name) AS target_name, m.target_node_id, m.target_existing,
                    m.status, m.step, m.main,
                    m.promote_phase, m.promote_detail, m.new_public_url, m.steps, m.error,
                    coalesce(o.status = 'pending_deletion', false) AS source_pending_deletion,
                    m.dns_changes, m.stale_tcp,
                    m.created_at, m.finished_at
             FROM server_migrations m
             LEFT JOIN provisioned_servers t ON t.id = m.target_server_id
             LEFT JOIN nodes tn ON tn.id = m.target_node_id
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
    m.target_location = m.target_location_id.as_deref().and_then(locations::get);
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
    /// None: shared by every workspace.
    node_workspace: Option<Id>,
    arch: Option<String>,
    mesh: bool,
}

async fn source(state: &AppState, node: Id) -> AppResult<Source> {
    sqlx::query_as(
        "SELECT n.name, coalesce(n.info->'capabilities' ? 'dashboard', false) AS main,
                p.id AS server_id, p.workspace_id AS server_workspace, p.status AS server_status,
                coalesce(p.location, n.location) AS location, p.ssh_enabled,
                n.workspace_id AS node_workspace, n.info->>'arch' AS arch,
                n.wg_public_key IS NOT NULL AS mesh
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
/// its provider server by IP first (as resizing does), with every connected
/// provider. Best effort: a main server not found anywhere (added by hand,
/// another account) migrates as a hand-added source.
async fn link_main(state: &AppState, node: Id, src: &mut Source, actor: Id) -> AppResult<()> {
    if src.server_id.is_none()
        && crate::provision::resize::link_node(state, node, actor)
            .await?
            .is_ok()
    {
        *src = source(state, node).await?;
    }
    Ok(())
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
    /// The provisioned server, None for one added by hand.
    pub server_id: Option<Id>,
    /// Whose providers pay for a new server: the linked server's, the node's
    /// own, or the one the request named.
    pub workspace_id: Option<Id>,
    /// A hand-added shared server: the request must name the workspace.
    pub workspace_required: bool,
    /// Providers the workspace has connected (where a new server can go).
    pub connected_providers: Vec<&'static str>,
    /// `x86` or `arm`, when the server has reported it.
    pub source_arch: Option<String>,
    /// What to know before starting; `blocking` ones refuse the migration.
    pub warnings: Vec<PreviewWarning>,
    /// Servers in the workspace (or shared) it could move to instead; empty
    /// for the main server, which needs a fresh machine.
    pub existing_targets: Vec<ExistingTarget>,
    pub location: Option<&'static Location>,
    pub services: Vec<PreviewService>,
    pub total_volume_bytes: i64,
    pub active_migration_id: Option<Id>,
    /// DNS records on this server's address in the workspace's zones.
    pub dns: super::dns::DnsPreview,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewWarning {
    pub code: &'static str,
    pub message: String,
    pub blocking: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingTarget {
    pub node_id: Id,
    pub name: String,
    pub location: Option<&'static Location>,
    pub arch: Option<String>,
    pub free_cpu_millis: i64,
    pub free_memory_mb: i64,
    /// Why it can't be chosen; None when it can.
    pub blocker: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PreviewQuery {
    /// For a hand-added server: whose providers and DNS zones to use.
    pub workspace_id: Option<Id>,
    /// A new target to check the move against.
    pub location: Option<String>,
    pub server_type: Option<String>,
    pub disk_gb: Option<u32>,
    /// An existing target to check the move against.
    pub target_node_id: Option<Id>,
}

/// The workspace a migration of `src` acts for: its linked server's, else
/// the one named, else the node's own. A node private to another workspace
/// can't be migrated on this one's behalf.
fn resolve_workspace(src: &Source, named: Option<Id>) -> AppResult<Option<Id>> {
    if let Some(linked) = src.server_workspace {
        if named.is_some_and(|n| n != linked) {
            return Err(AppError::bad_request(
                "this server belongs to another workspace",
            ));
        }
        return Ok(Some(linked));
    }
    if let (Some(own), Some(n)) = (src.node_workspace, named)
        && own != n
    {
        return Err(AppError::Conflict(
            "this server belongs to another workspace".into(),
        ));
    }
    Ok(named.or(src.node_workspace))
}

/// `x86` or `arm`, from a node's reported architecture or a catalog's.
fn normalize_arch(arch: &str) -> &str {
    match arch.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x86" => "x86",
        "aarch64" | "arm64" | "arm" => "arm",
        _ => "",
    }
}

/// Bytes of volume data that would move (the service's own volumes on this
/// server), measured so far.
async fn moving_bytes(state: &AppState, node: Id) -> AppResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT coalesce(sum(u.used_bytes), 0)::bigint FROM volumes v
         JOIN service_instances i ON i.id = v.service_instance_id
         LEFT JOIN volume_usage u ON u.volume_id = v.id
         WHERE i.node_id = $1",
    )
    .bind(node)
    .fetch_one(&state.db)
    .await?)
}

/// What is known about the target, for the warnings and the checks.
#[derive(Default)]
struct TargetFacts {
    arch: Option<String>,
    /// Its disk's size and what is free of it (known for provider servers).
    disk_gb: Option<u32>,
    free_bytes: Option<i64>,
    mesh: bool,
}

fn gb_bytes(gb: u32) -> i64 {
    i64::from(gb) * 1_000_000_000
}

/// Why the volumes can't move to a disk this size.
fn disk_blocker(moving: i64, facts: &TargetFacts) -> Option<String> {
    let free = facts.free_bytes?;
    (moving > free).then(|| {
        format!(
            "the volumes ({:.1} GB) won't fit on the target's disk ({:.1} GB free): choose a larger disk or server",
            moving as f64 / 1e9,
            free as f64 / 1e9,
        )
    })
}

/// A new server's facts, from its provider's catalog.
async fn new_target_facts(
    state: &AppState,
    workspace: Id,
    location: &str,
    server_type: &str,
    disk_gb: Option<u32>,
) -> AppResult<TargetFacts> {
    let Some(kind) = crate::provision::kind_of_location(location) else {
        return Err(AppError::bad_request(format!(
            "choose a {} location",
            crate::provision::labels()
        )));
    };
    let provider = crate::provision::connected_provider(state, workspace, kind).await?;
    let types = provider.server_types().await?;
    let offer = types
        .iter()
        .find(|t| t.name == server_type && t.orderable_in(location))
        .ok_or_else(|| {
            AppError::bad_request("that server type is not available in this location")
        })?;
    let disk = disk_gb.unwrap_or(offer.disk_gb);
    Ok(TargetFacts {
        arch: Some(offer.arch.clone()),
        disk_gb: Some(disk),
        free_bytes: Some(gb_bytes(disk)),
        // A new server joins the private network.
        mesh: true,
    })
}

/// An existing server's facts; a provider that can't be asked leaves the
/// disk unknown.
async fn existing_target_facts(state: &AppState, c: &Candidate) -> AppResult<TargetFacts> {
    let mut facts = TargetFacts {
        arch: c.arch.clone(),
        mesh: c.mesh,
        ..Default::default()
    };
    let (Some(kind), Some(pid), Some(ws)) = (&c.provider, &c.provider_id, c.server_workspace)
    else {
        return Ok(facts);
    };
    let Ok(Some(provider)) = crate::provision::provider_for(state, ws, kind).await else {
        return Ok(facts);
    };
    if let Ok(Some(server)) = provider.describe(pid).await {
        let used = moving_bytes(state, c.id).await?;
        facts.disk_gb = Some(server.disk_gb);
        facts.free_bytes = Some(gb_bytes(server.disk_gb) - used);
    }
    Ok(facts)
}

/// A server that might become an existing target.
#[derive(sqlx::FromRow)]
struct Candidate {
    id: Id,
    name: String,
    location: Option<String>,
    workspace_id: Option<Id>,
    public_ip: Option<String>,
    arch: Option<String>,
    mesh: bool,
    server_status: Option<String>,
    server_type: Option<String>,
    provider: Option<String>,
    provider_id: Option<String>,
    server_workspace: Option<Id>,
    /// It is being migrated, was, or is a migration's target.
    busy: bool,
}

macro_rules! select_candidates {
    ($rest:literal) => {
        concat!(
            "SELECT n.id, n.name, n.location, n.workspace_id, n.public_ip, n.info->>'arch' AS arch,
                    n.wg_public_key IS NOT NULL AS mesh,
                    p.status AS server_status, p.server_type, p.provider, p.provider_id,
                    p.workspace_id AS server_workspace,
                    EXISTS (SELECT 1 FROM server_migrations m
                            WHERE (m.source_node_id = n.id AND m.status IN ('running', 'failed', 'aborting', 'done'))
                               OR (m.target_node_id = n.id AND m.status IN ('running', 'failed', 'aborting'))) AS busy
             FROM nodes n
             LEFT JOIN LATERAL (SELECT * FROM provisioned_servers ps
                                WHERE ps.node_id = n.id AND ps.status NOT IN ('failed', 'deleting')
                                ORDER BY ps.created_at DESC LIMIT 1) p ON true
             WHERE n.status <> 'revoked' ",
            $rest
        )
    };
}

/// Why `c` can't take over from another server for `workspace`.
fn target_blocker(c: &Candidate, online: bool, workspace: Id) -> Option<String> {
    if c.workspace_id.is_some_and(|w| w != workspace) {
        return Some(format!("{} belongs to another workspace", c.name));
    }
    if !online {
        return Some(format!("{} is offline", c.name));
    }
    if c.busy {
        return Some(format!(
            "{} is part of another migration; choose another server",
            c.name
        ));
    }
    if c.server_status.as_deref().is_some_and(|s| s != "ready") {
        return Some(format!(
            "{} is being created, resized or deleted; wait until it is ready",
            c.name
        ));
    }
    if c.public_ip.is_none() {
        return Some(format!("{} hasn't reported a public IP yet", c.name));
    }
    None
}

/// What the services on `source` reserve: the limits placement counts, the
/// replicas it runs included.
async fn needed(state: &AppState, source: Id) -> AppResult<(i64, i64)> {
    use crate::provision::suggest::{DEFAULT_CPU_MILLIS, DEFAULT_MEMORY_MB};
    Ok(sqlx::query_as(
        "SELECT coalesce(sum(coalesce((i.config->>'cpuMillis')::bigint, $2)), 0)::bigint,
                coalesce(sum(coalesce((i.config->>'memoryMb')::bigint, $3)), 0)::bigint
         FROM service_instances i
         WHERE i.node_id = $1
            OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                       WHERE d.service_instance_id = i.id AND rp.node_id = $1
                         AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed'))",
    )
    .bind(source)
    .bind(DEFAULT_CPU_MILLIS)
    .bind(DEFAULT_MEMORY_MB)
    .fetch_one(&state.db)
    .await?)
}

fn room_blocker(
    name: &str,
    room: Option<&crate::provision::suggest::NodeRoom>,
    (cpu, memory): (i64, i64),
) -> Option<String> {
    match room {
        Some(r) if r.free_cpu_millis >= cpu && r.free_memory_mb >= memory => None,
        _ => Some(format!(
            "{name} doesn't have room for what runs here ({cpu} mCPU, {memory} MB of limits)"
        )),
    }
}

/// Services running on both servers: one replica per server, so they can't
/// be merged onto the target.
async fn replica_clash(state: &AppState, source: Id, target: Id) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT s.name FROM service_instances i JOIN services s ON s.id = i.service_id
         WHERE (i.node_id = $1 OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                                          WHERE d.service_instance_id = i.id AND rp.node_id = $1
                                            AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')))
           AND (i.node_id = $2 OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                                          WHERE d.service_instance_id = i.id AND rp.node_id = $2
                                            AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')))
         ORDER BY s.name",
    )
    .bind(source)
    .bind(target)
    .fetch_all(&state.db)
    .await?)
}

/// Every check an existing target has to pass but the transactional ones.
async fn check_existing(
    state: &AppState,
    source: Id,
    c: &Candidate,
    workspace: Id,
) -> AppResult<Option<String>> {
    if let Some(why) = target_blocker(c, state.hub.is_connected(c.id), workspace) {
        return Ok(Some(why));
    }
    let room = crate::provision::suggest::room(&state.db, workspace, None).await?;
    if let Some(why) = room_blocker(
        &c.name,
        room.iter().find(|r| r.node_id == c.id),
        needed(state, source).await?,
    ) {
        return Ok(Some(why));
    }
    let clash = replica_clash(state, source, c.id).await?;
    Ok((!clash.is_empty()).then(|| {
        format!(
            "{} already runs part of {}: replicas go one per server, so choose a server that doesn't",
            c.name,
            clash.join(", ")
        )
    }))
}

const MAIN_NEEDS_NEW: &str = "the main server can only move to a new server: promotion installs the control plane onto a fresh machine";

/// Local backups and PITR archives stay on the old server's disk.
async fn left_behind(state: &AppState, node: Id) -> AppResult<Vec<PreviewWarning>> {
    let mut out = Vec::new();
    let backups: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backups b JOIN backup_targets t ON t.id = b.target_id
         WHERE t.kind = 'local' AND t.node_id = $1 AND b.status = 'completed'",
    )
    .bind(node)
    .fetch_one(&state.db)
    .await?;
    if backups > 0 {
        out.push(PreviewWarning {
            code: "local_backups",
            message: format!(
                "{backups} backup(s) on this server's disk stay behind: they can only be restored while the database runs here, and go when it is deleted. Copy what you need first."
            ),
            blocking: false,
        });
    }
    let pitr: Vec<String> = sqlx::query_scalar(
        "SELECT s.name FROM pitr_configs c
         JOIN backup_targets t ON t.id = c.target_id
         JOIN service_instances i ON i.id = c.service_instance_id
         JOIN services s ON s.id = i.service_id
         WHERE c.enabled AND t.kind = 'local' AND t.node_id = $1 ORDER BY s.name",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    if !pitr.is_empty() {
        out.push(PreviewWarning {
            code: "local_pitr",
            message: format!(
                "Point-in-time recovery of {} archives to this server's disk: it stops after the move, and its history stays behind. Choose the new server's disk or object storage afterwards.",
                pitr.join(", ")
            ),
            blocking: false,
        });
    }
    Ok(out)
}

/// What a migration of this server would move: services, volume sizes and
/// TCP proxy addresses, for the confirmation dialog. With a target in the
/// query it also says what that target changes.
pub async fn preview(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    Query(q): Query<PreviewQuery>,
) -> AppResult<Json<Preview>> {
    let node = grant.id();
    let mut src = source(&state, node).await?;
    let main_blocker = if src.main {
        match main_blocker(&state) {
            Some(b) => Some(b),
            None => {
                link_main(&state, node, &mut src, actor.user_id).await?;
                None
            }
        }
    } else {
        None
    };
    let workspace = resolve_workspace(&src, q.workspace_id)?;
    // Naming a workspace reads its DNS zones and provider catalog.
    if let Some(w) = workspace
        && (q.workspace_id.is_some() || q.location.is_some())
    {
        require_workspace_admin(&state, &actor, w).await?;
    }
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
    let dns = super::dns::preview(&state, workspace, node).await;

    let mut warnings = left_behind(&state, node).await?;
    let proxies: Vec<&str> = services
        .iter()
        .filter_map(|s| s.tcp_address.as_deref())
        .collect();
    if !proxies.is_empty() {
        warnings.push(PreviewWarning {
            code: "tcp_address",
            message: format!(
                "TCP proxy addresses change with the server's IP ({}): clients using them must be updated.",
                proxies.join(", ")
            ),
            blocking: false,
        });
    }
    if src.server_id.is_none() {
        warnings.push(PreviewWarning {
            code: "hand_added",
            message: "This server was added by hand, so Modula can't manage its firewall: make sure the new server can reach it on UDP 51820 for the private network.".into(),
            blocking: false,
        });
    }
    let facts = match (workspace, &q.location, &q.server_type, q.target_node_id) {
        (Some(w), Some(l), Some(t), _) if !src.main || q.target_node_id.is_none() => {
            match new_target_facts(&state, w, l, t, q.disk_gb).await {
                Ok(f) => Some(f),
                Err(e) => {
                    warnings.push(PreviewWarning {
                        code: "target",
                        message: moves::error_text(&e),
                        blocking: true,
                    });
                    None
                }
            }
        }
        (Some(w), _, _, Some(target)) => {
            if src.main {
                warnings.push(PreviewWarning {
                    code: "target",
                    message: MAIN_NEEDS_NEW.into(),
                    blocking: true,
                });
                None
            } else {
                let c: Option<Candidate> =
                    sqlx::query_as(select_candidates!("AND n.id = $1 AND n.id <> $2"))
                        .bind(target)
                        .bind(node)
                        .fetch_optional(&state.db)
                        .await?;
                match c {
                    None => {
                        warnings.push(PreviewWarning {
                            code: "target",
                            message: "no such server".into(),
                            blocking: true,
                        });
                        None
                    }
                    Some(c) => {
                        if let Some(why) = check_existing(&state, node, &c, w).await? {
                            warnings.push(PreviewWarning {
                                code: "target",
                                message: why,
                                blocking: true,
                            });
                        }
                        Some(existing_target_facts(&state, &c).await?)
                    }
                }
            }
        }
        _ => None,
    };
    if let Some(f) = &facts {
        let from = src.arch.as_deref().map(normalize_arch).unwrap_or("");
        let to = f.arch.as_deref().map(normalize_arch).unwrap_or("");
        if !from.is_empty() && !to.is_empty() && from != to {
            warnings.push(PreviewWarning {
                code: "arch",
                message: format!(
                    "This moves from {from} to {to}: images built here are rebuilt only if they come from source; a prebuilt image must publish the {to} platform or it won't start."
                ),
                blocking: false,
            });
        }
        let moving = moving_bytes(&state, node).await?;
        match (disk_blocker(moving, f), f.free_bytes) {
            (Some(why), _) => warnings.push(PreviewWarning {
                code: "disk",
                message: why,
                blocking: true,
            }),
            (None, Some(free)) if moving * 10 > free * 8 => warnings.push(PreviewWarning {
                code: "disk",
                message: format!(
                    "The volumes ({:.1} GB) would nearly fill the target's disk ({:.1} GB free).",
                    moving as f64 / 1e9,
                    free as f64 / 1e9,
                ),
                blocking: false,
            }),
            _ => {}
        }
        if !f.mesh || !src.mesh {
            warnings.push(PreviewWarning {
                code: "no_mesh",
                message: "One of the servers isn't on the private network: the old server can't forward moved domains while DNS catches up.".into(),
                blocking: false,
            });
        }
    }

    let mut existing_targets = Vec::new();
    if let (Some(w), false) = (workspace, src.main) {
        let room = crate::provision::suggest::room(&state.db, w, None).await?;
        let need = needed(&state, node).await?;
        let candidates: Vec<Candidate> = sqlx::query_as(select_candidates!(
            "AND n.id <> $1 AND (n.workspace_id IS NULL OR n.workspace_id = $2) ORDER BY n.name"
        ))
        .bind(node)
        .bind(w)
        .fetch_all(&state.db)
        .await?;
        for c in candidates {
            let r = room.iter().find(|r| r.node_id == c.id);
            let blocker = match target_blocker(&c, state.hub.is_connected(c.id), w) {
                Some(b) => Some(b),
                None => room_blocker(&c.name, r, need),
            };
            existing_targets.push(ExistingTarget {
                node_id: c.id,
                name: c.name,
                location: c.location.as_deref().and_then(locations::get),
                arch: c.arch,
                free_cpu_millis: r.map_or(0, |r| r.free_cpu_millis),
                free_memory_mb: r.map_or(0, |r| r.free_memory_mb),
                blocker,
            });
        }
    }

    let connected_providers = match workspace {
        Some(w) => crate::provision::connected_kinds(&state.db, w).await?,
        None => Vec::new(),
    };
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
        workspace_required: workspace.is_none(),
        workspace_id: workspace,
        connected_providers,
        source_arch: src.arch.as_deref().map(|a| normalize_arch(a).to_owned()),
        warnings,
        existing_targets,
        location: src.location.as_deref().and_then(locations::get),
        services,
        total_volume_bytes: total,
        active_migration_id: active,
        dns,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Migrate {
    /// Whose providers and DNS zones to use; required for a shared server
    /// added by hand, and otherwise must be the server's own.
    #[serde(default)]
    pub workspace_id: Option<Id>,
    pub target: Target,
    /// DNS record ids (from the preview) to leave on the old address.
    #[serde(default)]
    pub dns_skip: Vec<String>,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Target {
    /// Create a server at any provider the workspace has connected.
    New {
        provider: String,
        /// Catalog id of its location, e.g. `hetzner:hel1`.
        location: String,
        server_type: String,
        /// Root disk size, for providers where it is chosen separately (EC2).
        #[serde(default)]
        disk_gb: Option<u32>,
    },
    /// Move to a server that already exists; it is never deleted here.
    Existing { node_id: Id },
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

struct NewMigration<'a> {
    id: Id,
    workspace: Id,
    node: Id,
    server: Option<Id>,
    name: &'a str,
    location: Option<&'a str>,
    server_type: Option<&'a str>,
    target_node: Option<Id>,
    step: &'a str,
    actor: Id,
    main: bool,
    dns_skip: &'a [String],
}

/// Serializes starting migrations that involve these servers (two migrations
/// could otherwise pick each other as targets), then refuses a source that
/// is another migration's target or was migrated already.
async fn claim_nodes(tx: &mut sqlx::PgConnection, nodes: &[Id]) -> AppResult<()> {
    let ids: Vec<String> = nodes.iter().map(ToString::to_string).collect();
    sqlx::query("SELECT 1 FROM nodes WHERE id::text = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM server_migrations
                        WHERE (target_node_id = $1 AND status IN ('running', 'failed', 'aborting'))
                           OR (source_node_id = $1 AND status = 'done'))",
    )
    .bind(nodes[0])
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(AppError::Conflict(
            "this server is the target of another migration, or has been migrated already".into(),
        ));
    }
    Ok(())
}

async fn insert_migration(tx: &mut sqlx::PgConnection, m: NewMigration<'_>) -> AppResult<()> {
    let inserted = sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_server_id, source_name,
                                        target_location, target_server_type, target_node_id,
                                        target_existing, step, created_by, steps, main, source_ip, dns_skip)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8 IS NOT NULL, $9, $10, '[]', $11,
                 (SELECT public_ip FROM nodes WHERE id = $3), $12)",
    )
    .bind(m.id)
    .bind(m.workspace)
    .bind(m.node)
    .bind(m.server)
    .bind(m.name)
    .bind(m.location)
    .bind(m.server_type)
    .bind(m.target_node)
    .bind(m.step)
    .bind(m.actor)
    .bind(m.main)
    .bind(sqlx::types::Json(m.dns_skip))
    .execute(&mut *tx)
    .await;
    match inserted {
        Err(sqlx::Error::Database(d)) if d.constraint() == Some("server_migrations_source_key") => {
            Err(AppError::Conflict(
                "this server is already being migrated; abort that migration first".into(),
            ))
        }
        r => r.map(drop).map_err(Into::into),
    }
}

/// Migrate a server: to a new one (created now through the provisioning path,
/// so the workspace's caps apply) or one that exists, moving everything in
/// the background.
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
        link_main(&state, node, &mut src, actor.user_id).await?;
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
    let Some(workspace) = resolve_workspace(&src, req.workspace_id)? else {
        return Err(AppError::bad_request(
            "this server was added by hand and is shared: name the workspace to migrate it for",
        ));
    };
    if src.server_status.as_deref().is_some_and(|s| s != "ready") {
        return Err(AppError::Conflict(
            "the server must be ready (not being created, resized or deleted) to migrate it".into(),
        ));
    }
    require_workspace_admin(&state, &actor, workspace).await?;
    if req.dns_skip.len() > 5000 || req.dns_skip.iter().any(|r| r.len() > 64) {
        return Err(AppError::bad_request("invalid DNS records to skip"));
    }
    if src.main && matches!(req.target, Target::Existing { .. }) {
        return Err(AppError::Conflict(MAIN_NEEDS_NEW.into()));
    }
    // A shared server's other tenants can't follow it to a server only this
    // workspace may use (the main server's successor becomes shared itself).
    if !src.main {
        let others: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT s.name FROM service_instances i
             JOIN services s ON s.id = i.service_id
             JOIN projects p ON p.id = s.project_id
             WHERE p.workspace_id <> $2
               AND (i.node_id = $1
                    OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                               WHERE d.service_instance_id = i.id AND rp.node_id = $1
                                 AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')))
             ORDER BY 1 LIMIT 6",
        )
        .bind(node)
        .bind(workspace)
        .fetch_all(&state.db)
        .await?;
        if !others.is_empty() {
            return Err(AppError::Conflict(format!(
                "services of other workspaces run on this server ({}): move them first",
                others.join(", ")
            )));
        }
    }
    let moving = moving_bytes(&state, node).await?;
    let id = Id::new();
    match req.target {
        Target::Existing { node_id: target } => {
            migrate_to_existing(
                &state,
                &actor,
                &meta,
                &src,
                node,
                workspace,
                id,
                target,
                moving,
                &req.dns_skip,
            )
            .await?;
        }
        Target::New {
            provider,
            location,
            server_type,
            disk_gb,
        } => {
            if crate::provision::kind_of_location(&location) != Some(provider.as_str()) {
                return Err(AppError::bad_request(format!(
                    "{location} is not a {} location",
                    crate::provision::label(&provider)
                )));
            }
            let facts =
                new_target_facts(&state, workspace, &location, &server_type, disk_gb).await?;
            if let Some(why) = disk_blocker(moving, &facts) {
                return Err(AppError::Conflict(why));
            }
            migrate_to_new(
                &state,
                &actor,
                &meta,
                &src,
                node,
                workspace,
                id,
                &location,
                &server_type,
                disk_gb,
                &req.dns_skip,
            )
            .await?;
        }
    }
    Ok(Json(load_migration(&state, id).await?))
}

#[allow(clippy::too_many_arguments)]
async fn migrate_to_existing(
    state: &AppState,
    actor: &Actor,
    meta: &RequestMeta,
    src: &Source,
    node: Id,
    workspace: Id,
    id: Id,
    target: Id,
    moving: i64,
    dns_skip: &[String],
) -> AppResult<()> {
    if target == node {
        return Err(AppError::bad_request(
            "a server can't be migrated onto itself",
        ));
    }
    let c: Candidate = sqlx::query_as(select_candidates!("AND n.id = $1"))
        .bind(target)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::bad_request("no such server"))?;
    if let Some(why) = check_existing(state, node, &c, workspace).await? {
        return Err(AppError::Conflict(why));
    }
    let facts = existing_target_facts(state, &c).await?;
    if let Some(why) = disk_blocker(moving, &facts) {
        return Err(AppError::Conflict(why));
    }
    let mut tx = state.db.begin().await?;
    claim_nodes(&mut tx, &[node, target]).await?;
    // Under the locks: nothing else may have claimed it meanwhile.
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM server_migrations
                        WHERE (source_node_id = $1 AND status IN ('running', 'failed', 'aborting', 'done'))
                           OR (target_node_id = $1 AND status IN ('running', 'failed', 'aborting')))",
    )
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(AppError::Conflict(format!(
            "{} is part of another migration; choose another server",
            c.name
        )));
    }
    insert_migration(
        &mut tx,
        NewMigration {
            id,
            workspace,
            node,
            server: src.server_id,
            name: &src.name,
            location: c.location.as_deref(),
            server_type: c.server_type.as_deref(),
            target_node: Some(target),
            step: "join",
            actor: actor.user_id,
            main: false,
            dns_skip,
        },
    )
    .await?;
    moves::begin(
        &mut *tx,
        Table::Migrations,
        id,
        &format!("Checking {}", c.name),
    )
    .await?;
    Entry::new("server.migrate", meta)
        .actor(actor)
        .workspace(workspace)
        .target("server_migration", id)
        .metadata(json!({
            "node": node,
            "from": src.name,
            "fromServer": src.server_id,
            "toNode": target,
            "name": c.name,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    server::enqueue(state, id, std::time::Duration::ZERO).await
}

#[allow(clippy::too_many_arguments)]
async fn migrate_to_new(
    state: &AppState,
    actor: &Actor,
    meta: &RequestMeta,
    src: &Source,
    node: Id,
    workspace: Id,
    id: Id,
    location: &str,
    server_type: &str,
    disk_gb: Option<u32>,
    dns_skip: &[String],
) -> AppResult<()> {
    let name = new_name(&src.name);
    let mut tx = state.db.begin().await?;
    claim_nodes(&mut tx, &[node]).await?;
    insert_migration(
        &mut tx,
        NewMigration {
            id,
            workspace,
            node,
            server: src.server_id,
            name: &src.name,
            location: Some(location),
            server_type: Some(server_type),
            target_node: None,
            step: "create",
            actor: actor.user_id,
            main: src.main,
            dns_skip,
        },
    )
    .await?;
    let city = locations::get(location).map_or(location, |l| l.city);
    moves::begin(
        &mut *tx,
        Table::Migrations,
        id,
        &format!("Creating {name} ({server_type}) in {city}"),
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
            location: location.to_owned(),
            server_type: server_type.to_owned(),
            name: name.clone(),
            ssh_key_ids: Vec::new(),
            ssh: src.ssh_enabled.unwrap_or(false),
            disk_gb,
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
    Entry::new("server.migrate", meta)
        .actor(actor)
        .workspace(workspace)
        .target("server_migration", id)
        .metadata(json!({
            "node": node,
            "from": src.name,
            "fromServer": src.server_id,
            "toServer": created.id,
            "name": name,
            "location": location,
            "serverType": server_type,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    server::enqueue(state, id, std::time::Duration::from_secs(5)).await
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

/// Try a migration's failed DNS step again.
pub async fn retry_dns(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<MigrationView>> {
    let id = grant.id();
    if !server::retry_dns(&state, id).await? {
        return Err(AppError::Conflict(
            "only a migration whose DNS step failed can retry it".into(),
        ));
    }
    Entry::new("server.migrate.dns.retry", &meta)
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
