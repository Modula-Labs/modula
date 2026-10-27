//! An environment's current problems, worst first, each with what fixes it
//! (the project page's Warnings panel): failed or crashed deployments,
//! services on an offline server, services on the main server, nearly full
//! disks and references to services or shared variables that don't exist.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::Serialize;

use crate::authz::Grant;
use crate::deploy::placement;
use crate::deploy::refs::{Ref, Target};
use crate::error::AppResult;
use crate::state::AppState;

const DOCS: &str = "https://usemodula.org/docs/deploy";
/// Warn when a disk has less than this share free.
const DISK_FREE_MIN: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// What clicking the warning opens.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Fix {
    /// The deployment's logs.
    #[serde(rename_all = "camelCase")]
    OpenDeployment { deployment_id: Id },
    /// The service's server picker, with this server (the recommended one)
    /// preselected; None when no other server is usable.
    #[serde(rename_all = "camelCase")]
    MoveServer { node_id: Option<Id> },
    /// Add a server.
    AddServer,
    /// The server's page.
    #[serde(rename_all = "camelCase")]
    OpenServer { node_id: Id },
    /// The service's variables (shared ones when there's no service).
    OpenVariables,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Warning {
    /// `deploy_failed`, `crashed`, `server_offline`, `main_server`,
    /// `disk_full` or `missing_reference`.
    pub kind: &'static str,
    pub severity: Severity,
    pub service_id: Option<Id>,
    pub service_instance_id: Option<Id>,
    pub title: String,
    pub detail: String,
    /// Why it matters and how to fix it.
    pub docs_url: String,
    pub action: Fix,
}

#[derive(Serialize)]
pub struct Warnings {
    pub items: Vec<Warning>,
}

#[derive(sqlx::FromRow)]
struct Row {
    instance: Id,
    service: Id,
    name: String,
    node: Option<Id>,
    node_name: Option<String>,
    node_online: Option<bool>,
    main: bool,
    has_volume: bool,
    deployment: Option<Id>,
    status: Option<String>,
    error: Option<String>,
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Warnings>> {
    Ok(Json(Warnings {
        items: compute(&state, grant.id()).await?,
    }))
}

pub async fn compute(state: &AppState, environment: Id) -> AppResult<Vec<Warning>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id AS instance, s.id AS service, s.name, i.node_id AS node,
                n.name AS node_name, n.status = 'online' AS node_online,
                coalesce(n.info->'capabilities' ? 'dashboard', false) AS main,
                EXISTS (SELECT 1 FROM volumes v WHERE v.service_instance_id = i.id) AS has_volume,
                d.id AS deployment, d.status, d.error
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         LEFT JOIN nodes n ON n.id = i.node_id AND n.status <> 'revoked'
         LEFT JOIN LATERAL (
             SELECT id, status, error FROM deployments
             WHERE service_instance_id = i.id AND status NOT IN ('cancelled', 'waiting_ci', 'queued')
             ORDER BY created_at DESC LIMIT 1) d ON true
         WHERE i.environment_id = $1
         ORDER BY s.name",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    let mut out = Vec::new();
    let mut conn = state.db.acquire().await?;
    let spots = placement::suggest(&mut conn, environment, None, &[]).await?;
    let workers = spots.iter().any(|s| !s.main && s.online);
    let mut on_main = Vec::new();

    for r in &rows {
        let service = |kind, severity, title: String, detail: String, docs: &str, action| Warning {
            kind,
            severity,
            service_id: Some(r.service),
            service_instance_id: Some(r.instance),
            title,
            detail,
            docs_url: format!("{DOCS}/{docs}"),
            action,
        };
        match (r.deployment, r.status.as_deref()) {
            (Some(d), Some("failed")) => out.push(service(
                "deploy_failed",
                Severity::Error,
                format!("{}: the latest deployment failed", r.name),
                r.error
                    .clone()
                    .unwrap_or_else(|| "See the build and deploy logs.".into()),
                "deployments/#troubleshooting",
                Fix::OpenDeployment { deployment_id: d },
            )),
            (Some(d), Some("crashed")) => out.push(service(
                "crashed",
                Severity::Error,
                format!("{} crashed", r.name),
                r.error
                    .clone()
                    .unwrap_or_else(|| "The app exited; see its runtime logs.".into()),
                "deployments/#troubleshooting",
                Fix::OpenDeployment { deployment_id: d },
            )),
            _ => {}
        }
        let (Some(node), Some(node_name)) = (r.node, r.node_name.as_deref()) else {
            continue;
        };
        if r.node_online == Some(false) {
            out.push(service(
                "server_offline",
                Severity::Error,
                format!("{}'s server {node_name} is offline", r.name),
                "It isn't reachable, so the service may be down and can't be deployed.".into(),
                "servers/#server-health",
                Fix::OpenServer { node_id: node },
            ));
        }
        if r.main {
            if workers {
                let target = placement::suggest(&mut conn, environment, Some(r.instance), &[])
                    .await?
                    .into_iter()
                    .find(|s| !s.main && s.online)
                    .map(|s| s.id);
                let mut detail = "The main server runs the control plane: an app compromise or \
                                  resource spike there affects the dashboard and every other \
                                  server. Move it to another server."
                    .to_owned();
                if r.has_volume {
                    detail.push_str(" Its volume's data is copied over, with downtime.");
                }
                out.push(service(
                    "main_server",
                    Severity::Warning,
                    format!("{} runs on the main server", r.name),
                    detail,
                    "servers/#keep-apps-off-the-main-server",
                    Fix::MoveServer { node_id: target },
                ));
            } else {
                on_main.push(r.name.clone());
            }
        }
    }
    // Only the main server: one note for them all (fine for a hobby setup).
    if !on_main.is_empty() {
        out.push(Warning {
            kind: "main_server",
            severity: Severity::Info,
            service_id: None,
            service_instance_id: None,
            title: if on_main.len() == 1 {
                format!("{} runs on the main server", on_main[0])
            } else {
                format!("{} services run on the main server", on_main.len())
            },
            detail: "It also runs the control plane. Add a server to keep apps apart from it."
                .into(),
            docs_url: format!("{DOCS}/servers/#keep-apps-off-the-main-server"),
            action: Fix::AddServer,
        });
    }

    // Nearly full disks (the latest sample of each, last hour) of the
    // environment's servers.
    let nodes: Vec<Id> = rows
        .iter()
        .filter_map(|r| r.node_name.as_ref().and(r.node))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let disks: Vec<(Id, String, String, i64, i64)> = sqlx::query_as(
        "SELECT DISTINCT ON (s.node_id, s.path) s.node_id, n.name, s.path, s.total_bytes, s.free_bytes
         FROM disk_samples s JOIN nodes n ON n.id = s.node_id
         WHERE s.node_id = ANY($1) AND s.at > now() - interval '1 hour'
         ORDER BY s.node_id, s.path, s.at DESC",
    )
    .bind(&nodes)
    .fetch_all(&state.db)
    .await?;
    let mut full = BTreeMap::<Id, (String, f64)>::new();
    for (node, name, _, total, free) in disks {
        if total <= 0 {
            continue;
        }
        let share = free as f64 / total as f64;
        if share < DISK_FREE_MIN {
            let e = full.entry(node).or_insert((name, share));
            e.1 = e.1.min(share);
        }
    }
    for (node, (name, free)) in full {
        let used = ((1.0 - free) * 100.0).round();
        let services: Vec<&str> = rows
            .iter()
            .filter(|r| r.node == Some(node))
            .map(|r| r.name.as_str())
            .collect();
        out.push(Warning {
            kind: "disk_full",
            severity: Severity::Warning,
            service_id: None,
            service_instance_id: None,
            title: format!("{name}'s disk is {used}% full"),
            detail: format!(
                "Builds, logs and volumes fail when it fills up. It runs {}.",
                services.join(", ")
            ),
            docs_url: format!("{DOCS}/servers/#server-health"),
            action: Fix::OpenServer { node_id: node },
        });
    }

    // References to services or shared variables that don't exist (they
    // resolve to ""). Platform variables aren't rows, so names on existing
    // services aren't checked.
    let vars: Vec<(Option<Id>, String, Vec<String>)> = sqlx::query_as(
        "SELECT service_instance_id, name, refs FROM variables
         WHERE environment_id = $1 AND cardinality(refs) > 0 ORDER BY name",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    let shared: BTreeSet<String> = sqlx::query_scalar(
        "SELECT name FROM variables WHERE environment_id = $1 AND service_instance_id IS NULL",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    let names: BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let mut missing = BTreeMap::<Option<Id>, Vec<String>>::new();
    for (owner, var, refs) in &vars {
        for r in refs.iter().filter_map(|r| Ref::parse(r)) {
            let gone = match &r.target {
                Target::Service(s) => !names.contains(s.as_str()),
                Target::Shared => !shared.contains(&r.name),
                // `${{NAME}}` in a shared variable is another shared one.
                Target::Own => owner.is_none() && !shared.contains(&r.name),
            };
            if gone {
                missing
                    .entry(*owner)
                    .or_default()
                    .push(format!("{var} → ${{{{{}}}}}", r.key()));
            }
        }
    }
    for (owner, list) in missing {
        let row = owner.and_then(|o| rows.iter().find(|r| r.instance == o));
        let whose = row.map_or("Shared variables".to_owned(), |r| r.name.clone());
        out.push(Warning {
            kind: "missing_reference",
            severity: Severity::Warning,
            service_id: row.map(|r| r.service),
            service_instance_id: owner,
            title: format!(
                "{whose}: {} reference{} to nothing",
                list.len(),
                if list.len() == 1 { "" } else { "s" }
            ),
            detail: format!("They resolve to an empty value: {}.", list.join(", ")),
            docs_url: format!("{DOCS}/variables/#references"),
            action: Fix::OpenVariables,
        });
    }
    out.sort_by_key(|w| w.severity);
    Ok(out)
}
