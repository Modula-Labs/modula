//! A project's activity feed: change sets and deployments, newest first.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use serde_json::{Value, json};

use crate::api::deployments::deploy_instance;
use crate::api::services::set_config;
use crate::api::variables::{self, UpdateVariables};
use crate::audit::Entry;
use crate::auth::session::Actor as Caller;
use crate::authz::{Grant, Role, Scope};
use crate::deploy::config::ServiceConfig;
use crate::deploy::env;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

const MAX_GROUPS: usize = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityQuery {
    pub environment_id: Option<Id>,
    pub service_id: Option<Id>,
    /// `deployment`, `variable`, `config` or `service`; all when absent.
    pub kind: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub items: Vec<Group>,
}

/// One request's changes, or one deployment.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: Id,
    pub service_id: Option<Id>,
    pub service_name: Option<String>,
    pub actor: Option<Actor>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub changes: Vec<Change>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub kind: String,
    pub op: String,
    pub key: Option<String>,
    pub status: Option<String>,
    pub deployment_id: Option<Id>,
    pub message: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct ChangeRow {
    set_id: Id,
    service_id: Option<Id>,
    service_name: Option<String>,
    actor: Option<String>,
    kind: String,
    op: String,
    key: Option<String>,
    at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: Id,
    service_id: Id,
    service_name: String,
    actor: Option<String>,
    avatar_url: Option<String>,
    status: String,
    commit_message: Option<String>,
    at: OffsetDateTime,
}

pub async fn list(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<ActivityQuery>,
) -> AppResult<Json<Activity>> {
    let project = grant.id();
    let kind = q.kind.as_deref();
    if !matches!(
        kind,
        None | Some("deployment" | "variable" | "config" | "service")
    ) {
        return Err(AppError::bad_request(
            "kind must be deployment, variable, config or service",
        ));
    }
    let mut items = Vec::new();

    if kind != Some("deployment") {
        let rows: Vec<ChangeRow> = sqlx::query_as(
            "SELECT cs.id AS set_id, c.service_id, s.name AS service_name, u.name AS actor,
                    c.kind, c.op, c.key, c.created_at AS at
             FROM change_sets cs
             JOIN environments e ON e.id = cs.environment_id
             JOIN changes c ON c.change_set_id = cs.id
             LEFT JOIN services s ON s.id = c.service_id
             LEFT JOIN users u ON u.id = cs.created_by
             WHERE e.project_id = $1 AND cs.status = 'applied'
               AND ($2::text IS NULL OR cs.environment_id = $2)
               AND ($3::text IS NULL OR c.service_id = $3)
               AND ($4::text IS NULL OR c.kind = $4)
             ORDER BY cs.created_at DESC, cs.id DESC, c.id
             LIMIT 1000",
        )
        .bind(project)
        .bind(q.environment_id)
        .bind(q.service_id)
        .bind(kind)
        .fetch_all(&state.db)
        .await?;
        for r in rows {
            let change = Change {
                kind: r.kind,
                op: r.op,
                key: r.key,
                status: None,
                deployment_id: None,
                message: None,
                at: r.at,
            };
            match items.last_mut() {
                Some(Group { id, changes, .. }) if *id == r.set_id => changes.push(change),
                _ => items.push(Group {
                    id: r.set_id,
                    service_id: r.service_id,
                    service_name: r.service_name,
                    actor: r.actor.map(|name| Actor {
                        name,
                        avatar_url: None,
                    }),
                    at: r.at,
                    changes: vec![change],
                }),
            }
        }
        items.truncate(MAX_GROUPS);
    }

    if matches!(kind, None | Some("deployment")) {
        let rows: Vec<DeploymentRow> = sqlx::query_as(
            "SELECT d.id, s.id AS service_id, s.name AS service_name,
                    coalesce(u.name, nullif(d.commit_author, '')) AS actor,
                    CASE WHEN d.created_by IS NULL AND d.commit_author_login IS NOT NULL
                         THEN 'https://avatars.githubusercontent.com/' || d.commit_author_login || '?s=64' END AS avatar_url,
                    d.status, coalesce(d.message, d.commit_message) AS commit_message, d.updated_at AS at
             FROM deployments d
             JOIN service_instances i ON i.id = d.service_instance_id
             JOIN services s ON s.id = i.service_id
             LEFT JOIN users u ON u.id = d.created_by
             WHERE s.project_id = $1
               AND ($2::text IS NULL OR i.environment_id = $2)
               AND ($3::text IS NULL OR s.id = $3)
             ORDER BY d.created_at DESC
             LIMIT 50",
        )
        .bind(project)
        .bind(q.environment_id)
        .bind(q.service_id)
        .fetch_all(&state.db)
        .await?;
        items.extend(rows.into_iter().map(|r| Group {
            id: r.id,
            service_id: Some(r.service_id),
            service_name: Some(r.service_name),
            actor: r.actor.map(|name| Actor {
                name,
                avatar_url: r.avatar_url,
            }),
            at: r.at,
            changes: vec![Change {
                kind: "deployment".into(),
                op: "status".into(),
                key: None,
                status: Some(r.status),
                deployment_id: Some(r.id),
                message: r.commit_message,
                at: r.at,
            }],
        }));
    }

    items.sort_by_key(|g| std::cmp::Reverse(g.at));
    items.truncate(MAX_GROUPS);
    Ok(Json(Activity { items }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub count: usize,
    pub services: Vec<PendingService>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingService {
    pub service_id: Id,
    pub service_name: String,
    pub instance_id: Id,
    pub changes: Vec<PendingChange>,
}

/// One variable or setting that differs from the service's last deployment
/// (several edits of the same key collapse into one).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChange {
    #[serde(flatten)]
    pub change: Change,
    /// The deployed value. Only with `?values=true` for Members and up, never
    /// for sealed variables; settings are JSON (strings unquoted).
    pub current_value: Option<String>,
    pub new_value: Option<String>,
    /// A sealed variable: its values are never returned.
    pub sealed: bool,
    /// A shared variable this service references (`${{shared.KEY}}`).
    pub shared: bool,
    /// Whether `POST …/discard` can revert this change on its own.
    pub discardable: bool,
}

/// A pending change with the deployed value kept for discarding.
struct Diff {
    out: PendingChange,
    /// What to restore: the deployed variable or setting (None = remove it).
    before_var: Option<env::RawVar>,
    before_config: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    service_id: Id,
    service_name: String,
    instance_id: Id,
    deployment_id: Option<Id>,
    shared: bool,
    kind: String,
    op: String,
    key: Option<String>,
    at: OffsetDateTime,
}

/// Changes since each service's last deployment in an environment: they take
/// effect on its next deploy. A shared variable's change counts for every
/// service that references it.
pub async fn pending_changes(state: &AppState, environment: Id) -> AppResult<Pending> {
    pending_with(state, environment, false).await
}

async fn pending_with(state: &AppState, environment: Id, values: bool) -> AppResult<Pending> {
    let services = diffs(state, environment)
        .await?
        .into_iter()
        .map(|(svc, diffs)| PendingService {
            changes: diffs
                .into_iter()
                .map(|d| {
                    let mut c = d.out;
                    if !values || c.sealed {
                        c.current_value = None;
                        c.new_value = None;
                    }
                    c
                })
                .collect(),
            ..svc
        })
        .collect::<Vec<_>>();
    let count = services.iter().map(|s| s.changes.len()).sum();
    Ok(Pending { count, services })
}

/// Every service's pending changes, compared against its last deployment's
/// snapshot (config, and its variables as written) so that edits reverted by
/// hand or discarded no longer count.
async fn diffs(state: &AppState, environment: Id) -> AppResult<Vec<(PendingService, Vec<Diff>)>> {
    let rows: Vec<PendingRow> = sqlx::query_as(
        "WITH last AS (
             SELECT i.id AS instance_id, i.service_id, s.name AS service_name, d.id AS deployment_id,
                    d.created_at AS deployed_at
             FROM service_instances i JOIN services s ON s.id = i.service_id
             LEFT JOIN LATERAL (SELECT id, created_at FROM deployments
                                WHERE service_instance_id = i.id ORDER BY created_at DESC LIMIT 1) d ON true
             WHERE i.environment_id = $1
         )
         SELECT l.service_id, l.service_name, l.instance_id, l.deployment_id, c.service_id IS NULL AS shared,
                c.kind, c.op, c.key, c.created_at AS at
         FROM last l
         JOIN change_sets cs ON cs.environment_id = $1 AND cs.status = 'applied'
         JOIN changes c ON c.change_set_id = cs.id
         WHERE (c.service_id = l.service_id
                OR (c.service_id IS NULL AND c.kind = 'variable' AND EXISTS (
                     SELECT 1 FROM variables v
                     WHERE v.service_instance_id = l.instance_id AND 'shared.' || c.key = ANY(v.refs))))
           AND (l.deployed_at IS NULL OR c.created_at > l.deployed_at)
         ORDER BY l.service_name, c.created_at, c.id",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;

    // Group by service, then collapse repeated edits of one key (keeping the
    // first op and the latest time).
    let mut grouped: Vec<(PendingRow, Vec<PendingRow>)> = Vec::new();
    for r in rows {
        match grouped.last_mut() {
            Some((first, list)) if first.service_id == r.service_id => {
                match list.iter_mut().find(|x| {
                    x.key.is_some() && x.kind == r.kind && x.key == r.key && x.shared == r.shared
                }) {
                    Some(x) => x.at = r.at,
                    None => list.push(r),
                }
            }
            _ => {
                let head = clone_row(&r);
                grouped.push((head, vec![r]));
            }
        }
    }

    let mut out = Vec::with_capacity(grouped.len());
    for (head, list) in grouped {
        let snapshot = Snapshot::load(state, environment, &head, &list).await?;
        let diffs: Vec<Diff> = list.into_iter().filter_map(|r| snapshot.diff(r)).collect();
        if diffs.is_empty() {
            continue;
        }
        out.push((
            PendingService {
                service_id: head.service_id,
                service_name: head.service_name,
                instance_id: head.instance_id,
                changes: Vec::new(),
            },
            diffs,
        ));
    }
    Ok(out)
}

fn clone_row(r: &PendingRow) -> PendingRow {
    PendingRow {
        service_id: r.service_id,
        service_name: r.service_name.clone(),
        instance_id: r.instance_id,
        deployment_id: r.deployment_id,
        shared: r.shared,
        kind: r.kind.clone(),
        op: r.op.clone(),
        key: r.key.clone(),
        at: r.at,
    }
}

/// A service's deployed and current state, loaded once per service.
struct Snapshot {
    deployed: bool,
    deployed_config: Option<Value>,
    config: Value,
    /// The deployed variables as written (empty when never deployed, or when
    /// no logged change needs them).
    deployed_vars: env::RawVars,
    vars: env::RawVars,
}

impl Snapshot {
    async fn load(
        state: &AppState,
        environment: Id,
        head: &PendingRow,
        list: &[PendingRow],
    ) -> AppResult<Self> {
        let config: Value =
            sqlx::query_scalar("SELECT config FROM service_instances WHERE id = $1")
                .bind(head.instance_id)
                .fetch_one(&state.db)
                .await?;
        let needs_vars = list.iter().any(|r| r.kind == "variable" && !r.shared);
        let (deployed_config, deployed_vars) = match head.deployment_id {
            Some(d) => {
                let (cfg, sealed): (Value, Option<Vec<u8>>) =
                    sqlx::query_as("SELECT config, vars_sealed FROM deployments WHERE id = $1")
                        .bind(d)
                        .fetch_one(&state.db)
                        .await?;
                let vars = match sealed {
                    Some(s) if needs_vars => {
                        env::open_vars_snapshot(&state.keys, &state.db, environment, d, &s).await?
                    }
                    _ => env::RawVars::new(),
                };
                (Some(cfg), vars)
            }
            // Never deployed: everything is new.
            None => (None, env::RawVars::new()),
        };
        let vars = if needs_vars {
            env::raw_variables(&state.keys, &state.db, environment, head.instance_id).await?
        } else {
            env::RawVars::new()
        };
        Ok(Self {
            deployed: head.deployment_id.is_some(),
            deployed_config,
            config,
            deployed_vars,
            vars,
        })
    }

    /// The net change for one logged key, or None when it is back to what
    /// was deployed.
    fn diff(&self, r: PendingRow) -> Option<Diff> {
        let logged = |discardable| Diff {
            out: PendingChange {
                change: change(r.kind.clone(), r.op.clone(), r.key.clone(), r.at),
                current_value: None,
                new_value: None,
                sealed: false,
                shared: r.shared,
                discardable,
            },
            before_var: None,
            before_config: None,
        };
        let Some(key) = r.key.as_deref() else {
            return Some(logged(false));
        };
        match r.kind.as_str() {
            // `service.NAME`: another service's variable this one references
            // (e.g. a regenerated database password); names never contain dots.
            "variable" if !r.shared && !key.contains('.') => {
                let (before, after) = (self.deployed_vars.get(key), self.vars.get(key));
                let op = net_op(before.map(|v| &v.value), after.map(|v| &v.value))?;
                Some(Diff {
                    out: PendingChange {
                        change: change(r.kind, op.into(), r.key, r.at),
                        current_value: before.map(|v| v.value.clone()),
                        new_value: after.map(|v| v.value.clone()),
                        sealed: before.is_some_and(|v| v.sealed) || after.is_some_and(|v| v.sealed),
                        shared: false,
                        discardable: true,
                    },
                    before_var: before.cloned(),
                    before_config: None,
                })
            }
            "config" => {
                let after = self.config.get(key);
                let Some(deployed) = &self.deployed_config else {
                    let mut d = logged(false);
                    d.out.new_value = after.map(display);
                    return Some(d);
                };
                let before = deployed.get(key);
                let op = net_op(before, after)?;
                Some(Diff {
                    out: PendingChange {
                        change: change(r.kind, op.into(), r.key, r.at),
                        current_value: before.map(display),
                        new_value: after.map(display),
                        sealed: false,
                        shared: false,
                        discardable: self.deployed,
                    },
                    before_var: None,
                    before_config: before.cloned(),
                })
            }
            _ => Some(logged(false)),
        }
    }
}

fn change(kind: String, op: String, key: Option<String>, at: OffsetDateTime) -> Change {
    Change {
        kind,
        op,
        key,
        status: None,
        deployment_id: None,
        message: None,
        at,
    }
}

/// `added` / `removed` / `updated`, or None when nothing changed.
fn net_op<T: PartialEq>(before: Option<T>, after: Option<T>) -> Option<&'static str> {
    match (before, after) {
        (None, Some(_)) => Some("added"),
        (Some(_), None) => Some("removed"),
        (Some(a), Some(b)) if a != b => Some("updated"),
        _ => None,
    }
}

/// A setting's value for display: strings as-is, anything else as JSON.
fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingQuery {
    /// Include current and new values (Members and up; sealed never).
    #[serde(default)]
    pub values: bool,
}

pub async fn pending(
    State(state): State<AppState>,
    actor: Caller,
    grant: Grant,
    meta: RequestMeta,
    Query(q): Query<PendingQuery>,
) -> AppResult<Json<Pending>> {
    let environment = grant.id();
    let values =
        q.values && matches!(grant.scope, Scope::Workspace { role, .. } if role >= Role::Member);
    let pending = pending_with(&state, environment, values).await?;
    if values && pending.count > 0 {
        Entry::new("environment.pending_read", &meta)
            .actor(&actor)
            .workspace(grant.workspace_id()?)
            .target("environment", environment)
            .record(&state.db)
            .await?;
    }
    Ok(Json(pending))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscardKey {
    /// `variable` or `config`.
    pub kind: String,
    pub key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscardPending {
    pub service_id: Id,
    /// The changes to discard; all of the service's when absent. Shared
    /// variables are left alone (they belong to every service using them).
    pub changes: Option<Vec<DiscardKey>>,
}

/// Revert pending changes of one service to its last deployment: a changed
/// or deleted variable gets its deployed value back (sealed ones re-sealed,
/// never returned), an added one is deleted, a setting gets its deployed
/// value. Goes through the normal write paths, so it shows in the activity
/// feed like any edit.
pub async fn discard(
    State(state): State<AppState>,
    actor: Caller,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<DiscardPending>,
) -> AppResult<Json<Pending>> {
    let environment = grant.id();
    let Some((svc, diffs)) = diffs(&state, environment)
        .await?
        .into_iter()
        .find(|(s, _)| s.service_id == req.service_id)
    else {
        return Err(AppError::NotFound);
    };
    let picked: Vec<Diff> = match &req.changes {
        None => diffs.into_iter().filter(|d| d.out.discardable).collect(),
        Some(keys) => {
            let mut picked = Vec::with_capacity(keys.len());
            let mut diffs = diffs;
            for k in keys {
                let i = diffs
                    .iter()
                    .position(|d| {
                        !d.out.shared
                            && d.out.change.kind == k.kind
                            && d.out.change.key.as_deref() == Some(&k.key)
                    })
                    .ok_or_else(|| {
                        AppError::bad_request(format!(
                            "{} has no pending change to {}",
                            svc.service_name, k.key
                        ))
                    })?;
                let d = diffs.swap_remove(i);
                if !d.out.discardable {
                    return Err(AppError::Conflict(format!(
                        "{} can't be discarded; change it back by hand",
                        k.key
                    )));
                }
                picked.push(d);
            }
            picked
        }
    };
    if picked.is_empty() {
        return pending_with(&state, environment, false).await.map(Json);
    }

    let mut vars = UpdateVariables {
        set: Default::default(),
        sealed: Default::default(),
        delete: Default::default(),
    };
    let mut config: Option<Value> = None;
    let mut keys = Vec::with_capacity(picked.len());
    for d in picked {
        let key = d.out.change.key.clone().unwrap_or_default();
        keys.push(json!({ "kind": d.out.change.kind, "key": key }));
        match d.out.change.kind.as_str() {
            "variable" => match d.before_var {
                Some(v) => {
                    if v.sealed {
                        vars.sealed.insert(key.clone());
                    }
                    vars.set.insert(key, v.value);
                }
                None => {
                    vars.delete.insert(key);
                }
            },
            "config" => {
                let cfg = match &mut config {
                    Some(c) => c,
                    None => config.insert(
                        sqlx::query_scalar("SELECT config FROM service_instances WHERE id = $1")
                            .bind(svc.instance_id)
                            .fetch_one(&state.db)
                            .await?,
                    ),
                };
                let Some(obj) = cfg.as_object_mut() else {
                    return Err(AppError::Internal(anyhow::anyhow!(
                        "config is not an object"
                    )));
                };
                match d.before_config {
                    Some(v) => obj.insert(key, v),
                    None => obj.remove(&key),
                };
            }
            _ => {}
        }
    }
    let workspace = grant.workspace_id()?;
    if let Some(cfg) = config {
        let cfg: ServiceConfig = serde_json::from_value(cfg).map_err(|e| {
            AppError::Conflict(format!("the deployed settings can't be restored: {e}"))
        })?;
        set_config(&state, &actor, workspace, &meta, svc.instance_id, &cfg).await?;
    }
    if !vars.set.is_empty() || !vars.delete.is_empty() {
        variables::apply_to_instance(&state, &actor, &grant, &meta, svc.instance_id, vars).await?;
    }
    Entry::new("pending.discard", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", svc.instance_id)
        .metadata(json!({ "changes": keys }))
        .record(&state.db)
        .await?;
    pending_with(&state, environment, false).await.map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeployPending {
    /// Services to deploy; default: every service with pending changes.
    pub service_ids: Option<Vec<Id>>,
    /// Shown on each deployment and in the activity feed.
    pub message: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployed {
    pub service_id: Id,
    pub deployment_id: Option<Id>,
    pub error: Option<String>,
}

const MAX_MESSAGE_CHARS: usize = 500;

/// "Deploy N changes": deploy each service's current config. Services deploy
/// independently; one failing to start (e.g. GitHub unreachable) doesn't stop
/// the others.
pub async fn deploy(
    State(state): State<AppState>,
    actor: Caller,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<DeployPending>,
) -> AppResult<Json<serde_json::Value>> {
    let environment = grant.id();
    let message = req
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if message.is_some_and(|m| m.chars().count() > MAX_MESSAGE_CHARS) {
        return Err(AppError::bad_request(format!(
            "the message must be at most {MAX_MESSAGE_CHARS} characters"
        )));
    }
    let targets: Vec<(Id, Id)> = match &req.service_ids {
        Some(ids) => sqlx::query_as(
            "SELECT service_id, id FROM service_instances WHERE environment_id = $1 AND service_id = ANY($2)",
        )
        .bind(environment)
        .bind(ids)
        .fetch_all(&state.db)
        .await?,
        None => pending_changes(&state, environment)
            .await?
            .services
            .into_iter()
            .map(|s| (s.service_id, s.instance_id))
            .collect(),
    };
    let workspace = grant.workspace_id()?;
    let mut out = Vec::with_capacity(targets.len());
    for (service, instance) in targets {
        match deploy_instance(&state, &actor, workspace, &meta, instance, message).await {
            Ok(id) => out.push(Deployed {
                service_id: service,
                deployment_id: Some(id),
                error: None,
            }),
            Err(e) => out.push(Deployed {
                service_id: service,
                deployment_id: None,
                error: Some(e.public_message()),
            }),
        }
    }
    Ok(Json(serde_json::json!({ "deployments": out })))
}
