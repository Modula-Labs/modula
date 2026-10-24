//! Postgres read replicas.
//!
//! A replica is its own service (template `postgres`, own volume) whose
//! `replica_of` is the primary's service. Its credentials are references to
//! the primary's variables (`pg_basebackup` copies the primary's roles), and
//! desired state gives its deployments `MODULA_REPLICA_OF`, so the template's
//! start script fills its empty data directory with `pg_basebackup -R` over
//! the private network and runs it as a streaming standby (read-only). It
//! connects as `modula_replicator`, a role this module creates on the primary
//! with the primary's `REPLICATION_PASSWORD`. The primary gets
//! `DATABASE_REPLICA_URL`, the replica keeps its own `DATABASE_URL`.
//!
//! Promoting runs `pg_promote()`, drops the link, and turns the referenced
//! credentials into the replica's own values, so it no longer follows the
//! primary's.

use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::database;
use crate::api::services::{CreateService, create_service, validate_service_name};
use crate::api::variables;
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::env;
use crate::deploy::placement::Candidate;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// The replica's variables that follow the primary's until it is promoted.
pub const FOLLOWED: [&str; 4] = [
    "POSTGRES_USER",
    "POSTGRES_PASSWORD",
    "POSTGRES_DB",
    "REPLICATION_PASSWORD",
];
const TIMEOUT: Duration = Duration::from_secs(30);

/// The followed variables as references to `primary`'s.
pub fn followed_vars(primary: &str) -> Vec<(String, String)> {
    FOLLOWED
        .iter()
        .map(|k| ((*k).to_owned(), format!("${{{{{primary}.{k}}}}}")))
        .collect()
}

/// Where a replica runs: another online server in the private network,
/// the least loaded; else (no such server) the primary's own.
pub fn replica_node(candidates: &[Candidate], primary: Id) -> Id {
    crate::deploy::placement::pick(
        &candidates
            .iter()
            .filter(|c| c.id != primary)
            .cloned()
            .collect::<Vec<_>>(),
        1,
    )
    .and_then(|n| n.first().copied())
    .unwrap_or(primary)
}

struct Instance {
    service: Id,
    service_name: String,
    project: Id,
    environment: Id,
    node: Option<Id>,
    template: Option<String>,
    replica_of: Option<Id>,
}

async fn instance(state: &AppState, id: Id) -> AppResult<Instance> {
    #[allow(clippy::type_complexity)]
    let r: (Id, String, Id, Id, Option<Id>, Option<String>, Option<Id>) = sqlx::query_as(
        "SELECT s.id, s.name, s.project_id, i.environment_id, i.node_id, s.template, s.replica_of
         FROM service_instances i JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Instance {
        service: r.0,
        service_name: r.1,
        project: r.2,
        environment: r.3,
        node: r.4,
        template: r.5,
        replica_of: r.6,
    })
}

fn postgres(i: &Instance) -> AppResult<()> {
    if i.template.as_deref() != Some("postgres") {
        return Err(AppError::Conflict(
            "read replicas are for Postgres services".into(),
        ));
    }
    Ok(())
}

/// Set (replace) one variable of an instance, as written.
async fn set_variable(
    state: &AppState,
    environment: Id,
    instance: Id,
    name: &str,
    value: &str,
) -> AppResult<()> {
    let sealed =
        variables::seal_new(state, environment, &[(name.to_owned(), value.to_owned())]).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM variables WHERE service_instance_id = $1 AND name = $2")
        .bind(instance)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    variables::insert_new(&mut tx, environment, instance, &sealed).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewReplica {
    /// Default `<primary>-replica`.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedReplica {
    pub service_id: Id,
    pub service_name: String,
    pub instance_id: Id,
    pub node_id: Id,
}

/// Add a read replica of a running Postgres instance.
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    body: Option<Json<NewReplica>>,
) -> AppResult<Json<CreatedReplica>> {
    let primary = grant.id();
    let workspace = grant.workspace_id()?;
    let p = instance(&state, primary).await?;
    postgres(&p)?;
    if p.replica_of.is_some() {
        return Err(AppError::Conflict(
            "this is a replica: add replicas to its primary".into(),
        ));
    }
    let primary_node = p
        .node
        .ok_or_else(|| AppError::Conflict("deploy the database first".into()))?;
    let name = match body
        .and_then(|Json(b)| b.name)
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
    {
        Some(n) => n,
        None => format!(
            "{}-replica",
            p.service_name[..p.service_name.len().min(55)].trim_end_matches('-')
        ),
    };
    validate_service_name(&name)?;

    // The role replicas stream as, with the primary's REPLICATION_PASSWORD.
    let resolved = env::resolve(
        &state.keys,
        &state.db,
        state.cfg.tcp_proxy_host.as_deref(),
        p.environment,
        primary,
    )
    .await?;
    let password = resolved
        .vars
        .get("REPLICATION_PASSWORD")
        .map(|v| v.expose_secret().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AppError::Conflict(
                "the primary has no REPLICATION_PASSWORD variable (add one of 16–128 letters and digits)"
                    .into(),
            )
        })?;
    let live = database::live(&state, primary).await?;
    database::call(
        &state,
        &live,
        "replicationRole",
        json!({ "password": password }),
        TIMEOUT,
    )
    .await?;

    // Every environment's replica instance follows the primary's there.
    let envs: Vec<Id> = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
        .bind(p.project)
        .fetch_all(&state.db)
        .await?;
    let overrides: HashMap<Id, Vec<(String, String)>> = envs
        .into_iter()
        .map(|e| (e, followed_vars(&p.service_name)))
        .collect();
    let created = create_service(
        &state,
        &actor,
        &meta,
        workspace,
        p.project,
        CreateService {
            name,
            config: None,
            template: Some("postgres".into()),
        },
        &overrides,
    )
    .await?;
    sqlx::query("UPDATE services SET replica_of = $2 WHERE id = $1")
        .bind(created.id)
        .bind(p.service)
        .execute(&state.db)
        .await?;
    let replica = created
        .instances
        .iter()
        .find(|i| i.environment_id == p.environment)
        .map(|i| i.id)
        .ok_or(AppError::NotFound)?;

    // Another server when there is one in the private network.
    let mut tx = state.db.begin().await?;
    let candidates: Vec<Candidate> = sqlx::query_as(
        "SELECT n.id, n.name, n.status = 'online' AS online,
                (SELECT count(*) FROM deployment_replicas r JOIN deployments d ON d.id = r.deployment_id
                 WHERE r.node_id = n.id AND d.status IN ('building', 'deploying', 'active', 'crashed')) AS load,
                false AS previous
         FROM nodes n
         WHERE n.status <> 'revoked' AND n.mesh_index IS NOT NULL AND n.wg_public_key IS NOT NULL
           AND (n.workspace_id IS NULL OR n.workspace_id = $1)
           AND EXISTS (SELECT 1 FROM nodes pn WHERE pn.id = $2 AND pn.mesh_index IS NOT NULL)",
    )
    .bind(workspace)
    .bind(primary_node)
    .fetch_all(&mut *tx)
    .await?;
    let node = replica_node(&candidates, primary_node);
    sqlx::query("UPDATE service_instances SET node_id = $2 WHERE id = $1")
        .bind(replica)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    Entry::new("replica.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", primary)
        .metadata(json!({ "replica": replica, "service": created.id, "node": node }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    set_variable(
        &state,
        p.environment,
        primary,
        "DATABASE_REPLICA_URL",
        &format!("${{{{{}.DATABASE_URL}}}}", created.name),
    )
    .await?;
    crate::api::deployments::deploy_instance(
        &state,
        &actor,
        workspace,
        &meta,
        replica,
        Some(&format!("Read replica of {}", p.service_name)),
    )
    .await?;
    Ok(Json(CreatedReplica {
        service_id: created.id,
        service_name: created.name,
        instance_id: replica,
        node_id: node,
    }))
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub service_id: Id,
    pub service_name: String,
    pub instance_id: Option<Id>,
    pub node_id: Option<Id>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Replication {
    /// `primary` (has replicas), `replica`, or `standalone`.
    pub role: &'static str,
    pub primary: Option<Peer>,
    pub replicas: Vec<Peer>,
    /// What the running database reports (`pg_stat_replication`, or a
    /// standby's replay lag); None when it is not running.
    pub status: Option<Value>,
}

/// Replication of an instance: its primary or its replicas, and their lag.
pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Replication>> {
    let id = grant.id();
    let i = instance(&state, id).await?;
    postgres(&i)?;
    let primary: Option<Peer> = sqlx::query_as(
        "SELECT s.id AS service_id, s.name AS service_name, pi.id AS instance_id,
                pi.node_id
         FROM services s
         LEFT JOIN service_instances pi ON pi.service_id = s.id AND pi.environment_id = $2
         WHERE s.id = $1",
    )
    .bind(i.replica_of)
    .bind(i.environment)
    .fetch_optional(&state.db)
    .await?;
    let replicas: Vec<Peer> = sqlx::query_as(
        "SELECT s.id AS service_id, s.name AS service_name, ri.id AS instance_id, ri.node_id
         FROM services s
         LEFT JOIN service_instances ri ON ri.service_id = s.id AND ri.environment_id = $2
         WHERE s.replica_of = $1 ORDER BY s.name",
    )
    .bind(i.service)
    .bind(i.environment)
    .fetch_all(&state.db)
    .await?;
    let status = match database::live(&state, id).await {
        Ok(live) => database::call(&state, &live, "replication", Value::Null, TIMEOUT)
            .await
            .ok(),
        Err(_) => None,
    };
    Ok(Json(Replication {
        role: match (&primary, replicas.is_empty()) {
            (Some(_), _) => "replica",
            (None, false) => "primary",
            (None, true) => "standalone",
        },
        primary,
        replicas,
        status,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromoteBody {
    /// The replica's service name, typed to confirm.
    pub confirm: String,
}

/// Turn a replica into a standalone primary.
pub async fn promote(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<PromoteBody>,
) -> AppResult<Json<Replication>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    let i = instance(&state, id).await?;
    postgres(&i)?;
    let Some(primary) = i.replica_of else {
        return Err(AppError::Conflict("this database is not a replica".into()));
    };
    if req.confirm != i.service_name {
        return Err(AppError::bad_request(format!(
            "type the service's name ({}) to promote it",
            i.service_name
        )));
    }
    let live = database::live(&state, id).await?;
    database::call(
        &state,
        &live,
        "promote",
        Value::Null,
        Duration::from_secs(75),
    )
    .await?;
    // Its own credentials from now on (the values they had), in every
    // environment.
    let instances: Vec<(Id, Id)> =
        sqlx::query_as("SELECT id, environment_id FROM service_instances WHERE service_id = $1")
            .bind(i.service)
            .fetch_all(&state.db)
            .await?;
    for (inst, environment) in instances {
        let resolved = env::resolve(
            &state.keys,
            &state.db,
            state.cfg.tcp_proxy_host.as_deref(),
            environment,
            inst,
        )
        .await?;
        for name in FOLLOWED {
            if let Some(v) = resolved.vars.get(name) {
                set_variable(&state, environment, inst, name, v.expose_secret()).await?;
            }
        }
    }
    let primary_name: String = sqlx::query_scalar("SELECT name FROM services WHERE id = $1")
        .bind(primary)
        .fetch_one(&state.db)
        .await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE services SET replica_of = NULL WHERE id = $1")
        .bind(i.service)
        .execute(&mut *tx)
        .await?;
    // The primary's DATABASE_REPLICA_URL pointed here.
    sqlx::query(
        "DELETE FROM variables v USING service_instances pi
         WHERE v.service_instance_id = pi.id AND pi.service_id = $1
           AND v.name = 'DATABASE_REPLICA_URL' AND $2 = ANY (v.refs)",
    )
    .bind(primary)
    .bind(format!("{}.DATABASE_URL", i.service_name))
    .execute(&mut *tx)
    .await?;
    Entry::new("replica.promote", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", id)
        .metadata(json!({ "primary": primary_name }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    get(State(state), grant).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, online: bool, load: i64) -> Candidate {
        Candidate {
            id: Id::new(),
            name: name.into(),
            online,
            load,
            previous: false,
        }
    }

    #[test]
    fn replicas_prefer_another_server() {
        let primary = c("db-1", true, 0);
        let (busy, idle, off) = (c("b", true, 7), c("c", true, 2), c("d", false, 0));
        let all = [primary.clone(), busy.clone(), idle.clone(), off.clone()];
        assert_eq!(replica_node(&all, primary.id), idle.id);
        assert_eq!(
            replica_node(&[primary.clone(), off], primary.id),
            primary.id
        );
        assert_eq!(replica_node(&[], primary.id), primary.id);
    }

    #[test]
    fn credentials_follow_the_primary() {
        let vars: HashMap<String, String> = followed_vars("postgres").into_iter().collect();
        assert_eq!(vars["POSTGRES_PASSWORD"], "${{postgres.POSTGRES_PASSWORD}}");
        assert_eq!(
            vars["REPLICATION_PASSWORD"],
            "${{postgres.REPLICATION_PASSWORD}}"
        );
        assert_eq!(vars.len(), 4);
    }
}
