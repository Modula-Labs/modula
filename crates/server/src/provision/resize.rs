//! Changing a server's size in place, through its cloud provider.
//!
//! A node is linked to its provider server by a `provisioned_servers` row:
//! one created from the dashboard, or one *adopted* here on demand: the
//! node's public IP is looked up with every connected provider, and the
//! server found is recorded (`origin = 'adopted'`). That is how the main
//! server (created by `modula server create`) and servers joined by hand
//! become resizable.
//!
//! A resize is a step machine persisted in the row and driven by a
//! self-re-enqueueing job ([`RESIZE_JOB`]), each run one short step:
//! `shutdown` (graceful; after [`SHUTDOWN_GRACE`] `poweroff`) →
//! `change_type` (the provider action, disk kept so the server can be
//! downsized again) → `poweron` → `reconnect` (the node said hello again) →
//! `ready`. On failure the server is powered back on and the row returns to
//! `ready` with the error.
//!
//! The main server runs this control plane, so it can't switch itself off
//! and carry on: the API refuses it and `modula server resize` does the
//! same steps from the user's computer. Its new type is picked up the next
//! time the options are read ([`options`] refreshes it from the provider).

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::{ActionState, Phase, Power, ProviderServer, ServerProvider, ServerTypeOffer};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::integrations::hetzner::{self, Caps};
use crate::jobs::{self, Job, NewJob};
use crate::locations::{self, Location};
use crate::state::AppState;

pub const RESIZE_JOB: &str = "provision.resize";
const POLL: Duration = Duration::from_secs(5);
/// How long the operating system gets to shut down before power is cut.
pub const SHUTDOWN_GRACE: time::Duration = time::Duration::seconds(90);
const POWEROFF_WAIT: time::Duration = time::Duration::minutes(2);
const CHANGE_TYPE_WAIT: time::Duration = time::Duration::minutes(30);
const POWERON_WAIT: time::Duration = time::Duration::minutes(5);
const RECONNECT_WAIT: time::Duration = time::Duration::minutes(10);
/// A resize still going after this (e.g. the provider unreachable) is given up.
const RESIZE_MAX: time::Duration = time::Duration::hours(1);
/// Integration kinds that are server providers.
const PROVIDER_KINDS: &[&str] = &[hetzner::KIND];
/// What the main server is resized with.
pub const CLI_COMMAND: &str = "modula server resize";

/// The workspace's provider of `kind`, or None when not connected.
pub async fn provider_for(
    state: &AppState,
    workspace: Id,
    kind: &str,
) -> AppResult<Option<Box<dyn ServerProvider>>> {
    Ok(match kind {
        hetzner::KIND => hetzner::provider(state, workspace)
            .await?
            .map(|p| Box::new(p) as Box<dyn ServerProvider>),
        _ => None,
    })
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    name: String,
    status: String,
    public_ip: Option<String>,
    workspace_id: Option<Id>,
    location: Option<String>,
    /// Runs the control plane (the dashboard).
    main: bool,
}

async fn node(state: &AppState, id: Id) -> AppResult<NodeRow> {
    sqlx::query_as(
        "SELECT name, status, public_ip, workspace_id, location,
                coalesce(info->'capabilities' ? 'dashboard', false) AS main
         FROM nodes WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Linked {
    id: Id,
    workspace_id: Id,
    node_id: Option<Id>,
    provider: String,
    provider_id: Option<String>,
    location: String,
    server_type: String,
    monthly_price_cents: i32,
    status: String,
    error: Option<String>,
    origin: String,
    resize_from: Option<String>,
    resize_to: Option<String>,
    resize_step: Option<String>,
    resize_action_id: Option<String>,
    resize_step_at: Option<OffsetDateTime>,
    resize_started_at: Option<OffsetDateTime>,
}

macro_rules! select_linked {
    ($rest:literal) => {
        concat!(
            "SELECT id, workspace_id, node_id, provider, provider_id, location, server_type,
                    monthly_price_cents, status, error, origin, resize_from, resize_to, resize_step,
                    resize_action_id, resize_step_at, resize_started_at
             FROM provisioned_servers ",
            $rest
        )
    };
}

async fn linked_to_node(state: &AppState, node: Id) -> AppResult<Option<Linked>> {
    Ok(sqlx::query_as(select_linked!(
        "WHERE node_id = $1 AND provider_id IS NOT NULL
         AND status NOT IN ('failed', 'deleting') ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(node)
    .fetch_optional(&state.db)
    .await?)
}

async fn linked(state: &AppState, id: Id) -> AppResult<Option<Linked>> {
    Ok(sqlx::query_as(select_linked!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?)
}

/// A node and its provider server, or why it can't be managed from here.
enum Resolved {
    Managed(Box<Managed>),
    Unmanaged(String),
}

struct Managed {
    row: Linked,
    provider: Box<dyn ServerProvider>,
    server: ProviderServer,
}

/// Find the node's provider server: through its linked row, else by its
/// public IP with every connected provider (adopting what is found). Nodes
/// belong to the whole instance, so every workspace's connection is tried,
/// the node's own workspace first.
async fn resolve(state: &AppState, node_id: Id, node: &NodeRow, actor: Id) -> AppResult<Resolved> {
    if let Some(row) = linked_to_node(state, node_id).await? {
        let Some(provider) = provider_for(state, row.workspace_id, &row.provider).await? else {
            return Ok(Resolved::Unmanaged(
                "the cloud provider this server was found with is no longer connected; connect it again in workspace settings".into(),
            ));
        };
        let pid = row.provider_id.clone().unwrap_or_default();
        let Some(server) = provider.describe(&pid).await? else {
            return Ok(Resolved::Unmanaged(
                "this server no longer exists at the cloud provider".into(),
            ));
        };
        let row = refresh(state, row, provider.as_ref(), &server).await?;
        return Ok(Resolved::Managed(Box::new(Managed {
            row,
            provider,
            server,
        })));
    }

    let Some(ip) = node.public_ip.clone() else {
        return Ok(Resolved::Unmanaged(
            "this server hasn't reported its public IP yet".into(),
        ));
    };
    let candidates: Vec<(Id, String)> = sqlx::query_as(
        "SELECT workspace_id, kind FROM integrations WHERE kind = ANY($1)
         ORDER BY workspace_id = $2 DESC NULLS LAST, created_at",
    )
    .bind(PROVIDER_KINDS)
    .bind(node.workspace_id)
    .fetch_all(&state.db)
    .await?;
    if candidates.is_empty() {
        return Ok(Resolved::Unmanaged(
            "no cloud provider is connected: connect one (e.g. Hetzner) in workspace settings to resize servers from here".into(),
        ));
    }
    let mut last_error = None;
    for (workspace, kind) in candidates {
        let provider = match provider_for(state, workspace, &kind).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                last_error = Some(e.to_string());
                continue;
            }
        };
        match provider.find_by_address(&ip).await {
            Ok(Some(server)) => {
                let row = adopt(
                    state,
                    node_id,
                    node,
                    workspace,
                    provider.as_ref(),
                    &server,
                    actor,
                )
                .await?;
                return Ok(Resolved::Managed(Box::new(Managed {
                    row,
                    provider,
                    server,
                })));
            }
            Ok(None) => {}
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    Ok(Resolved::Unmanaged(match last_error {
        Some(e) => format!("couldn't look this server up at the cloud provider: {e}"),
        None => format!(
            "no connected cloud provider has a Modula server with IP {ip} (it was created elsewhere, or with another account)"
        ),
    }))
}

/// Link `node_id` to its provider server (adopting it by IP when needed):
/// the row's id, or why it can't be.
pub async fn link_node(state: &AppState, node_id: Id, actor: Id) -> AppResult<Result<Id, String>> {
    let n = node(state, node_id).await?;
    Ok(match resolve(state, node_id, &n, actor).await? {
        Resolved::Managed(m) => Ok(m.row.id),
        Resolved::Unmanaged(why) => Err(why),
    })
}

/// The monthly price of `server_type` in `location_id`, if it is sold there.
async fn price_of(
    provider: &dyn ServerProvider,
    server_type: &str,
    location_id: &str,
) -> Option<i64> {
    provider
        .server_types()
        .await
        .ok()?
        .iter()
        .find(|t| t.name == server_type)
        .and_then(|t| t.price_in(location_id))
}

fn cents(v: Option<i64>, fallback: i32) -> i32 {
    v.and_then(|c| i32::try_from(c).ok()).unwrap_or(fallback)
}

/// Record a server found by IP, linked to the node.
async fn adopt(
    state: &AppState,
    node_id: Id,
    node: &NodeRow,
    workspace: Id,
    provider: &dyn ServerProvider,
    server: &ProviderServer,
    actor: Id,
) -> AppResult<Linked> {
    let location = server
        .location
        .map(|l| l.id.to_owned())
        .or_else(|| node.location.clone())
        .unwrap_or_default();
    let price = cents(price_of(provider, &server.server_type, &location).await, 0);
    // A row created here for the same server (e.g. still joining) is linked
    // instead of duplicated.
    let id: Id = sqlx::query_scalar(
        "INSERT INTO provisioned_servers
           (id, workspace_id, provider, provider_id, name, location, server_type,
            monthly_price_cents, status, origin, ipv4, node_id, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'ready', 'adopted', $9, $10, $11)
         ON CONFLICT (provider, provider_id) DO UPDATE SET node_id = EXCLUDED.node_id, updated_at = now()
         RETURNING id",
    )
    .bind(Id::new())
    .bind(workspace)
    .bind(provider.kind())
    .bind(&server.id)
    .bind(&node.name)
    .bind(&location)
    .bind(&server.server_type)
    .bind(price)
    .bind(&server.ipv4)
    .bind(node_id)
    .bind(actor)
    .fetch_one(&state.db)
    .await?;
    linked(state, id).await?.ok_or(AppError::NotFound)
}

/// Keep the row's type and price in line with the provider's (e.g. after
/// `modula server resize` changed the main server).
async fn refresh(
    state: &AppState,
    row: Linked,
    provider: &dyn ServerProvider,
    server: &ProviderServer,
) -> AppResult<Linked> {
    if row.status == "resizing"
        || server.server_type.is_empty()
        || server.server_type == row.server_type
    {
        return Ok(row);
    }
    let price = cents(
        price_of(provider, &server.server_type, &row.location).await,
        row.monthly_price_cents,
    );
    sqlx::query(
        "UPDATE provisioned_servers SET server_type = $2, monthly_price_cents = $3, updated_at = now()
         WHERE id = $1 AND status <> 'resizing'",
    )
    .bind(row.id)
    .bind(&server.server_type)
    .bind(price)
    .execute(&state.db)
    .await?;
    Ok(Linked {
        server_type: server.server_type.clone(),
        monthly_price_cents: price,
        ..row
    })
}

// ------------------------------------------------------------------ options

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeOptions {
    /// False when no connected provider knows this server (see `reason`).
    pub managed: bool,
    pub reason: Option<String>,
    /// The control plane runs on it: resized with [`CLI_COMMAND`] only.
    pub main: bool,
    pub cli_command: Option<String>,
    pub provider: Option<String>,
    /// The linked `provisioned_servers` row.
    pub server_id: Option<Id>,
    /// The provider's id for the server.
    pub provider_id: Option<String>,
    pub location: Option<&'static Location>,
    pub current: Option<CurrentSize>,
    pub types: Vec<TypeOption>,
    pub resize: ResizeStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentSize {
    pub name: String,
    pub cores: u32,
    pub memory_gb: f64,
    /// The disk it has (kept across resizes).
    pub disk_gb: u32,
    pub arch: String,
    pub monthly_cents: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeOption {
    pub name: String,
    pub description: String,
    pub cores: u32,
    pub memory_gb: f64,
    pub disk_gb: u32,
    pub arch: String,
    pub cpu_type: String,
    pub current: bool,
    /// The price here, when it can be ordered here.
    pub monthly_cents: Option<i64>,
    /// Orderable in the server's location now.
    pub available_here: bool,
    /// Where it can be ordered instead (moving the server there is a migration).
    pub available_in: Vec<&'static Location>,
    /// The server can change to it in place (architecture, storage, disk).
    pub compatible: bool,
    /// Why not, when not compatible.
    pub blocker: Option<String>,
    /// `up`, `down` or `same`, by price here (else memory).
    pub direction: &'static str,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResizeStatus {
    /// The linked row's status (`ready`, `resizing`, …), if linked.
    pub status: Option<String>,
    pub step: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// The last resize's error, if it failed.
    pub error: Option<String>,
}

fn status_of(row: Option<&Linked>) -> ResizeStatus {
    let Some(r) = row else {
        return ResizeStatus::default();
    };
    ResizeStatus {
        status: Some(r.status.clone()),
        step: r.resize_step.clone(),
        from: r.resize_from.clone(),
        to: r.resize_to.clone(),
        started_at: r.resize_started_at,
        error: r.error.clone(),
    }
}

fn unmanaged(node: &NodeRow, reason: String) -> ResizeOptions {
    ResizeOptions {
        managed: false,
        reason: Some(reason),
        main: node.main,
        cli_command: node.main.then(|| CLI_COMMAND.to_owned()),
        provider: None,
        server_id: None,
        provider_id: None,
        location: None,
        current: None,
        types: Vec::new(),
        resize: ResizeStatus::default(),
    }
}

fn type_option(
    provider: &dyn ServerProvider,
    server: &ProviderServer,
    location_id: &str,
    current_price: Option<i64>,
    t: &ServerTypeOffer,
) -> TypeOption {
    let here = t.price_in(location_id);
    let current = t.name == server.server_type;
    let blocker = (!current)
        .then(|| provider.resize_blocker(server, t))
        .flatten();
    let direction = if current {
        "same"
    } else {
        match (here, current_price) {
            (Some(a), Some(b)) if a != b => {
                if a > b {
                    "up"
                } else {
                    "down"
                }
            }
            _ if t.memory_gb > server.memory_gb || t.cores > server.cores => "up",
            _ if t.memory_gb < server.memory_gb || t.cores < server.cores => "down",
            _ => "same",
        }
    };
    TypeOption {
        name: t.name.clone(),
        description: t.description.clone(),
        cores: t.cores,
        memory_gb: t.memory_gb,
        disk_gb: t.disk_gb,
        arch: t.arch.clone(),
        cpu_type: t.cpu_type.clone(),
        current,
        monthly_cents: here,
        available_here: here.is_some(),
        available_in: if here.is_some() {
            Vec::new()
        } else {
            t.prices
                .iter()
                .filter_map(|p| locations::get(p.location))
                .collect()
        },
        compatible: blocker.is_none(),
        blocker,
        direction,
    }
}

/// `GET /api/nodes/{node_id}/resize-options`: the server's size, the sizes
/// it can change to here, and where the others are sold. Links the node to
/// its provider server on first use.
pub async fn options(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
) -> AppResult<Json<ResizeOptions>> {
    let node_id = grant.id();
    let node = node(&state, node_id).await?;
    let (row, provider, server) = match resolve(&state, node_id, &node, actor.user_id).await? {
        Resolved::Managed(m) => (m.row, m.provider, m.server),
        Resolved::Unmanaged(reason) => return Ok(Json(unmanaged(&node, reason))),
    };
    let location_id = server
        .location
        .map(|l| l.id.to_owned())
        .unwrap_or_else(|| row.location.clone());
    let offers = provider.server_types().await?;
    let current_price = offers
        .iter()
        .find(|t| t.name == server.server_type)
        .and_then(|t| t.price_in(&location_id))
        .or(Some(i64::from(row.monthly_price_cents)).filter(|c| *c > 0));
    let mut types: Vec<TypeOption> = offers
        .iter()
        .map(|t| type_option(provider.as_ref(), &server, &location_id, current_price, t))
        .collect();
    types.sort_by(|a, b| {
        (
            !a.available_here,
            a.monthly_cents.unwrap_or(i64::MAX),
            &a.name,
        )
            .cmp(&(
                !b.available_here,
                b.monthly_cents.unwrap_or(i64::MAX),
                &b.name,
            ))
    });
    Ok(Json(ResizeOptions {
        managed: true,
        reason: None,
        main: node.main,
        cli_command: node.main.then(|| CLI_COMMAND.to_owned()),
        provider: Some(row.provider.clone()),
        server_id: Some(row.id),
        provider_id: Some(server.id.clone()),
        location: locations::get(&location_id),
        current: Some(CurrentSize {
            name: server.server_type.clone(),
            cores: server.cores,
            memory_gb: server.memory_gb,
            disk_gb: server.disk_gb,
            arch: server.arch.clone(),
            monthly_cents: current_price,
        }),
        types,
        resize: status_of(Some(&row)),
    }))
}

/// `GET /api/nodes/{node_id}/resize`: progress only (no provider calls), for polling.
pub async fn status(State(state): State<AppState>, grant: Grant) -> AppResult<Json<ResizeStatus>> {
    let row: Option<Linked> = sqlx::query_as(select_linked!(
        "WHERE node_id = $1 AND provider_id IS NOT NULL ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(grant.id())
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(status_of(row.as_ref())))
}

// ------------------------------------------------------------------- resize

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Resize {
    pub server_type: String,
}

/// `POST /api/nodes/{node_id}/resize`: start changing a worker's size. The
/// server restarts (a few minutes of downtime for what runs on it).
pub async fn start(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Resize>,
) -> AppResult<Json<ResizeStatus>> {
    let node_id = grant.id();
    let node = node(&state, node_id).await?;
    if node.main {
        return Err(AppError::Conflict(format!(
            "{} runs the dashboard and can't switch itself off and back on; resize it from your computer with `{CLI_COMMAND} --type {}`",
            node.name, req.server_type
        )));
    }
    if node.status == "revoked" {
        return Err(AppError::NotFound);
    }
    let (row, provider, server) = match resolve(&state, node_id, &node, actor.user_id).await? {
        Resolved::Managed(m) => (m.row, m.provider, m.server),
        Resolved::Unmanaged(reason) => return Err(AppError::Conflict(reason)),
    };
    if row.status != "ready" {
        return Err(AppError::Conflict(format!(
            "the server is {}; wait until it is ready",
            row.status
        )));
    }
    if req.server_type == server.server_type {
        return Err(AppError::bad_request(format!(
            "the server already is a {}",
            req.server_type
        )));
    }
    let location_id = server
        .location
        .map(|l| l.id.to_owned())
        .unwrap_or_else(|| row.location.clone());
    let offers = provider.server_types().await?;
    let Some(target) = offers.iter().find(|t| t.name == req.server_type) else {
        return Err(AppError::bad_request(format!(
            "{} is not a server type that can be ordered",
            req.server_type
        )));
    };
    let Some(price) = target.price_in(&location_id) else {
        let elsewhere: Vec<&str> = target
            .prices
            .iter()
            .filter_map(|p| locations::get(p.location))
            .map(|l| l.city)
            .collect();
        return Err(AppError::bad_request(format!(
            "{} isn't available in this server's location{}",
            target.name,
            if elsewhere.is_empty() {
                String::new()
            } else {
                format!(
                    " (it is in {}; moving the server there is a migration)",
                    elsewhere.join(", ")
                )
            }
        )));
    };
    if let Some(why) = provider.resize_blocker(&server, target) {
        return Err(AppError::bad_request(why));
    }
    if row.origin == "provisioned" {
        check_caps(
            &state,
            row.workspace_id,
            &row.provider,
            price - i64::from(row.monthly_price_cents),
        )
        .await?;
    }

    let started = sqlx::query(
        "UPDATE provisioned_servers
         SET status = 'resizing', error = NULL, resize_from = $2, resize_to = $3,
             resize_step = 'shutdown', resize_action_id = NULL,
             resize_step_at = now(), resize_started_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'ready'",
    )
    .bind(row.id)
    .bind(&server.server_type)
    .bind(&target.name)
    .execute(&state.db)
    .await?;
    if started.rows_affected() == 0 {
        return Err(AppError::Conflict("a resize is already under way".into()));
    }
    let pid = server.id.clone();
    let off = if server.phase == Phase::Off {
        Ok(())
    } else {
        provider.power(&pid, Power::Off).await
    };
    if let Err(e) = off {
        clear(&state, row.id, "ready", None).await?;
        return Err(e.into());
    }
    Entry::new("server.resize", &meta)
        .actor(&actor)
        .workspace(row.workspace_id)
        .target("node", node_id)
        .metadata(json!({
            "provider": row.provider,
            "server": row.id,
            "providerId": pid,
            "from": server.server_type,
            "to": target.name,
            "monthlyCents": price,
        }))
        .record(&state.db)
        .await?;
    enqueue(&state, row.id, POLL).await?;
    let row = linked(&state, row.id).await?;
    Ok(Json(status_of(row.as_ref())))
}

/// A bigger server must stay within the workspace's spending cap.
async fn check_caps(state: &AppState, workspace: Id, kind: &str, extra: i64) -> AppResult<()> {
    if extra <= 0 || kind != hetzner::KIND {
        return Ok(());
    }
    let metadata: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT metadata FROM integrations WHERE workspace_id = $1 AND kind = $2",
    )
    .bind(workspace)
    .bind(kind)
    .fetch_optional(&state.db)
    .await?;
    let caps = metadata
        .as_ref()
        .map(Caps::from_metadata)
        .unwrap_or_default();
    let used = hetzner::usage(&state.db, workspace).await?;
    if used.monthly_cents + extra > caps.max_monthly_cents {
        return Err(AppError::Conflict(format!(
            "this size would bring the monthly total to €{:.2}, over the €{:.2} limit; raise it in settings",
            (used.monthly_cents + extra) as f64 / 100.0,
            caps.max_monthly_cents as f64 / 100.0
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------- job

pub async fn enqueue(state: &AppState, id: Id, after: Duration) -> AppResult<()> {
    jobs::enqueue(
        &state.db,
        NewJob {
            kind: RESIZE_JOB,
            payload: json!({ "id": id }),
            run_at: Some(OffsetDateTime::now_utc() + after),
            max_attempts: Some(3),
            dedupe_key: None,
        },
    )
    .await?;
    state.jobs_notify.notify_one();
    Ok(())
}

pub async fn resize_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let id: Id = serde_json::from_value(job.payload["id"].clone())?;
    let done = match poll_once(&state, id).await {
        Ok(done) => done,
        Err(e) => {
            // The provider (or the database) is briefly unreachable: try
            // again, but not forever.
            tracing::warn!(%id, error = ?e, "resize step failed; retrying");
            let started: Option<OffsetDateTime> = sqlx::query_scalar(
                "SELECT resize_started_at FROM provisioned_servers WHERE id = $1 AND status = 'resizing'",
            )
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .flatten();
            match started {
                Some(at) if OffsetDateTime::now_utc() - at > RESIZE_MAX => {
                    if let Some(row) = linked(&state, id).await? {
                        let provider = provider_for(&state, row.workspace_id, &row.provider)
                            .await
                            .ok()
                            .flatten();
                        fail(&state, &row, provider.as_deref(), &format!("{e}")).await?;
                    }
                    true
                }
                Some(_) => false,
                None => true,
            }
        }
    };
    if !done {
        enqueue(&state, id, POLL)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(())
}

async fn set_step(
    state: &AppState,
    id: Id,
    from: &str,
    to: &str,
    action: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE provisioned_servers
         SET resize_step = $3, resize_action_id = COALESCE($4, resize_action_id),
             resize_step_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'resizing' AND resize_step = $2",
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .bind(action)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Leave the resize: back to `status` with `error`.
async fn clear(state: &AppState, id: Id, status: &str, error: Option<&str>) -> AppResult<()> {
    sqlx::query(
        "UPDATE provisioned_servers
         SET status = $2, error = $3, resize_step = NULL, resize_action_id = NULL,
             resize_step_at = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Give up: power the server back on (whatever its type now) and record why.
async fn fail(
    state: &AppState,
    row: &Linked,
    provider: Option<&dyn ServerProvider>,
    why: &str,
) -> AppResult<()> {
    let to = row.resize_to.as_deref().unwrap_or("the new size");
    let mut message = format!("resizing to {to} failed: {why}");
    if let (Some(p), Some(pid)) = (provider, row.provider_id.as_deref()) {
        match p.describe(pid).await {
            Ok(Some(s)) => {
                if s.phase != Phase::Running
                    && let Err(e) = p.power(pid, Power::On).await
                {
                    message.push_str(&format!("; powering it back on failed too: {e}"));
                }
                if !s.server_type.is_empty() && s.server_type != row.server_type {
                    let price = cents(
                        price_of(p, &s.server_type, &row.location).await,
                        row.monthly_price_cents,
                    );
                    sqlx::query(
                        "UPDATE provisioned_servers SET server_type = $2, monthly_price_cents = $3 WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(&s.server_type)
                    .bind(price)
                    .execute(&state.db)
                    .await?;
                }
            }
            Ok(None) => message.push_str("; the server no longer exists at the provider"),
            Err(e) => message.push_str(&format!("; the provider couldn't be asked about it: {e}")),
        }
    }
    tracing::warn!(server = %row.id, "{message}");
    clear(state, row.id, "ready", Some(&message)).await
}

/// One step. Returns true once there is nothing left to do.
pub async fn poll_once(state: &AppState, id: Id) -> AppResult<bool> {
    let Some(row) = linked(state, id).await? else {
        return Ok(true);
    };
    if row.status != "resizing" {
        return Ok(true);
    }
    let Some(provider) = provider_for(state, row.workspace_id, &row.provider).await? else {
        fail(state, &row, None, "the cloud provider was disconnected").await?;
        return Ok(true);
    };
    let p = provider.as_ref();
    let pid = row.provider_id.clone().unwrap_or_default();
    let to = row.resize_to.clone().unwrap_or_default();
    let step = row.resize_step.clone().unwrap_or_default();
    let elapsed =
        OffsetDateTime::now_utc() - row.resize_step_at.unwrap_or_else(OffsetDateTime::now_utc);

    match step.as_str() {
        "shutdown" | "poweroff" => {
            let Some(s) = p.describe(&pid).await? else {
                fail(
                    state,
                    &row,
                    Some(p),
                    "the server was deleted at the provider",
                )
                .await?;
                return Ok(true);
            };
            if s.phase == Phase::Off {
                match p.resize(&pid, &to).await {
                    Ok(action) => set_step(state, id, &step, "change_type", Some(&action)).await?,
                    Err(e) => {
                        fail(state, &row, Some(p), &e.to_string()).await?;
                        return Ok(true);
                    }
                }
            } else if step == "shutdown" && elapsed > SHUTDOWN_GRACE {
                // The OS ignored the shutdown request: cut the power.
                p.power(&pid, Power::ForceOff).await?;
                set_step(state, id, "shutdown", "poweroff", None).await?;
            } else if step == "poweroff" && elapsed > POWEROFF_WAIT {
                fail(state, &row, Some(p), "the server did not power off").await?;
                return Ok(true);
            }
            Ok(false)
        }
        "change_type" => {
            let action = row.resize_action_id.clone().unwrap_or_default();
            match p.action(&action).await? {
                ActionState::Running if elapsed > CHANGE_TYPE_WAIT => {
                    fail(
                        state,
                        &row,
                        Some(p),
                        "the provider took too long to change the type",
                    )
                    .await?;
                    Ok(true)
                }
                ActionState::Running => Ok(false),
                ActionState::Failed(why) => {
                    fail(state, &row, Some(p), &why).await?;
                    Ok(true)
                }
                ActionState::Done => {
                    // Some providers start the server again themselves.
                    let running = p
                        .describe(&pid)
                        .await?
                        .is_some_and(|s| s.phase == Phase::Running);
                    if running {
                        set_step(state, id, "change_type", "reconnect", None).await?;
                    } else {
                        p.power(&pid, Power::On).await?;
                        set_step(state, id, "change_type", "poweron", None).await?;
                    }
                    Ok(false)
                }
            }
        }
        "poweron" => {
            let running = p
                .describe(&pid)
                .await?
                .is_some_and(|s| s.phase == Phase::Running);
            if running {
                set_step(state, id, "poweron", "reconnect", None).await?;
            } else if elapsed > POWERON_WAIT {
                fail(state, &row, Some(p), "the server did not start again").await?;
                return Ok(true);
            }
            Ok(false)
        }
        "reconnect" => {
            // A hello or heartbeat since the server started again (a node
            // only sends them over a live connection).
            let back: bool = match row.node_id {
                Some(node) => sqlx::query_scalar(
                    "SELECT status = 'online' AND last_seen_at > $2 FROM nodes WHERE id = $1",
                )
                .bind(node)
                .bind(row.resize_step_at)
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(false),
                // Nothing to wait for.
                None => true,
            };
            if back {
                let price = cents(
                    price_of(p, &to, &row.location).await,
                    row.monthly_price_cents,
                );
                sqlx::query(
                    "UPDATE provisioned_servers
                     SET server_type = $2, monthly_price_cents = $3, status = 'ready', error = NULL,
                         resize_step = NULL, resize_action_id = NULL, resize_step_at = NULL,
                         updated_at = now()
                     WHERE id = $1 AND status = 'resizing'",
                )
                .bind(id)
                .bind(&to)
                .bind(price)
                .execute(&state.db)
                .await?;
                return Ok(true);
            }
            if elapsed > RECONNECT_WAIT {
                // The server runs at its new size; only the node is missing.
                let price = cents(
                    price_of(p, &to, &row.location).await,
                    row.monthly_price_cents,
                );
                sqlx::query(
                    "UPDATE provisioned_servers SET server_type = $2, monthly_price_cents = $3 WHERE id = $1",
                )
                .bind(id)
                .bind(&to)
                .bind(price)
                .execute(&state.db)
                .await?;
                clear(
                    state,
                    id,
                    "ready",
                    Some(&format!(
                        "the server runs as {to} now, but modula-node on it hasn't reconnected; check `systemctl status modula-node` there"
                    )),
                )
                .await?;
                return Ok(true);
            }
            Ok(false)
        }
        other => {
            fail(state, &row, Some(p), &format!("unknown step {other:?}")).await?;
            Ok(true)
        }
    }
}
