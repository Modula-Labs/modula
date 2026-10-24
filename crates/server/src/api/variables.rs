//! Variables of a service instance, and the shared variables of an
//! environment (`${{shared.NAME}}`). Values are write-only through this API
//! except for an explicit, audited, sudo-gated reveal, and `run_env`
//! (`modula run`): an instance's resolved values, audited, without sealed
//! variables or any that reference one.
//!
//! References between variables are parsed on save and stored in clear
//! (`variables.refs`), so dependency edges and cycle checks never decrypt.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::activity::{self, Change};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::env;
use crate::deploy::refs::{self, Ref, VarKey};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// Platform variables every service gets (and others can reference).
pub const PLATFORM: &[&str] = &[
    "PORT",
    "MODULA_PRIVATE_DOMAIN",
    "MODULA_PUBLIC_DOMAIN",
    "MODULA_SERVICE_NAME",
    "MODULA_SERVICE_ID",
    // A service's own is its new deployment; another's, the one serving it.
    "MODULA_DEPLOYMENT_ID",
    "MODULA_ENVIRONMENT_NAME",
    "MODULA_ENVIRONMENT_ID",
    "MODULA_PROJECT_ID",
    "MODULA_TCP_PROXY_DOMAIN",
    "MODULA_TCP_PROXY_PORT",
];

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct VariableInfo {
    pub id: Id,
    pub name: String,
    pub sealed: bool,
    /// `service.NAME`, `shared.NAME` or `NAME` (same service).
    #[sqlx(rename = "refs")]
    pub references: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Which variables a request is about.
#[derive(Clone, Copy)]
struct Scope {
    environment: Id,
    /// None = the environment's shared variables.
    instance: Option<Id>,
    service: Option<Id>,
}

impl Scope {
    async fn of_instance(state: &AppState, instance: Id) -> AppResult<Self> {
        let (environment, service): (Id, Id) = sqlx::query_as(
            "SELECT environment_id, service_id FROM service_instances WHERE id = $1",
        )
        .bind(instance)
        .fetch_one(&state.db)
        .await?;
        Ok(Self {
            environment,
            instance: Some(instance),
            service: Some(service),
        })
    }

    fn shared(environment: Id) -> Self {
        Self {
            environment,
            instance: None,
            service: None,
        }
    }
}

async fn list(state: &AppState, scope: Scope) -> AppResult<Vec<VariableInfo>> {
    Ok(sqlx::query_as(
        "SELECT id, name, sealed, refs, updated_at FROM variables
         WHERE environment_id = $1 AND service_instance_id IS NOT DISTINCT FROM $2 ORDER BY name",
    )
    .bind(scope.environment)
    .bind(scope.instance)
    .fetch_all(&state.db)
    .await?)
}

/// Names and metadata only; values are never returned here.
pub async fn instance_variables(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<VariableInfo>>> {
    let scope = Scope::of_instance(&state, grant.id()).await?;
    list(&state, scope).await.map(Json)
}

pub async fn shared_variables(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<VariableInfo>>> {
    list(&state, Scope::shared(grant.id())).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateVariables {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Names (from `set`) to store as sealed: write-only, never revealable.
    #[serde(default)]
    pub sealed: BTreeSet<String>,
    #[serde(default)]
    pub delete: BTreeSet<String>,
}

pub async fn update_instance_variables(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateVariables>,
) -> AppResult<Json<Vec<VariableInfo>>> {
    let scope = Scope::of_instance(&state, grant.id()).await?;
    update(&state, &actor, &grant, &meta, scope, req)
        .await
        .map(Json)
}

pub async fn update_shared_variables(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateVariables>,
) -> AppResult<Json<Vec<VariableInfo>>> {
    update(
        &state,
        &actor,
        &grant,
        &meta,
        Scope::shared(grant.id()),
        req,
    )
    .await
    .map(Json)
}

/// Set and delete an instance's variables as `actor`, exactly like
/// `PATCH /api/service-instances/{id}/variables` (discarding pending changes).
pub(crate) async fn apply_to_instance(
    state: &AppState,
    actor: &Actor,
    grant: &Grant,
    meta: &RequestMeta,
    instance: Id,
    req: UpdateVariables,
) -> AppResult<()> {
    let scope = Scope::of_instance(state, instance).await?;
    update(state, actor, grant, meta, scope, req).await?;
    Ok(())
}

fn validate(req: &UpdateVariables) -> AppResult<()> {
    for (k, v) in &req.set {
        env::validate_name(k)?;
        env::validate_value(v)?;
    }
    for k in &req.delete {
        env::validate_name(k)?;
        if req.set.contains_key(k) {
            return Err(AppError::bad_request(format!(
                "{k} is both set and deleted"
            )));
        }
    }
    if let Some(k) = req.sealed.iter().find(|k| !req.set.contains_key(*k)) {
        return Err(AppError::bad_request(format!(
            "{k} is marked sealed but not set"
        )));
    }
    if req.set.len() + req.delete.len() > env::MAX_VARIABLES {
        return Err(AppError::bad_request("too many variables in one request"));
    }
    Ok(())
}

async fn update(
    state: &AppState,
    actor: &Actor,
    grant: &Grant,
    meta: &RequestMeta,
    scope: Scope,
    req: UpdateVariables,
) -> AppResult<Vec<VariableInfo>> {
    validate(&req)?;
    let existing: BTreeMap<String, Id> = sqlx::query_as::<_, (String, Id)>(
        "SELECT name, id FROM variables WHERE environment_id = $1 AND service_instance_id IS NOT DISTINCT FROM $2",
    )
    .bind(scope.environment)
    .bind(scope.instance)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    let new_names = req
        .set
        .keys()
        .filter(|k| !existing.contains_key(*k))
        .count();
    if existing.len() + new_names > env::MAX_VARIABLES {
        return Err(AppError::bad_request(format!(
            "at most {} variables per service or environment",
            env::MAX_VARIABLES
        )));
    }
    let new_refs: BTreeMap<&String, Vec<String>> = req
        .set
        .iter()
        .map(|(k, v)| (k, refs::references(v).iter().map(Ref::key).collect()))
        .collect();
    check_cycles(state, scope, &new_refs, &req.delete).await?;

    // Seal before the transaction (the keyring may need to create the
    // environment's data key). The row id is bound into each ciphertext.
    let mut rows = Vec::with_capacity(req.set.len());
    for (name, value) in &req.set {
        let id = existing.get(name).copied().unwrap_or_else(Id::new);
        let sealed =
            env::seal_variable(&state.keys, &state.db, scope.environment, id, value).await?;
        rows.push((id, name, sealed, req.sealed.contains(name), &new_refs[name]));
    }
    let mut tx = state.db.begin().await?;
    for (id, name, sealed, is_sealed, refs) in &rows {
        // Sealed is permanent. The id check guards against a concurrent insert
        // of the same name under a different id (its AAD would not match).
        let r = sqlx::query(
            "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, sealed, refs)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (environment_id, service_instance_id, name) DO UPDATE
               SET value_sealed = EXCLUDED.value_sealed, sealed = variables.sealed OR EXCLUDED.sealed,
                   refs = EXCLUDED.refs, updated_at = now()
               WHERE variables.id = EXCLUDED.id",
        )
        .bind(id)
        .bind(scope.environment)
        .bind(scope.instance)
        .bind(name)
        .bind(sealed)
        .bind(is_sealed)
        .bind(refs)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(AppError::Conflict(
                "variables changed concurrently; retry".into(),
            ));
        }
    }
    let deleted: Vec<&String> = req.delete.iter().collect();
    sqlx::query(
        "DELETE FROM variables
         WHERE environment_id = $1 AND service_instance_id IS NOT DISTINCT FROM $2 AND name = ANY($3)",
    )
    .bind(scope.environment)
    .bind(scope.instance)
    .bind(&deleted)
    .execute(&mut *tx)
    .await?;
    let changes: Vec<Change> = req
        .set
        .keys()
        .map(|k| {
            let op = if existing.contains_key(k) {
                "updated"
            } else {
                "added"
            };
            Change::new("variable", op, k.as_str())
        })
        .chain(
            deleted
                .iter()
                .filter(|k| existing.contains_key(k.as_str()))
                .map(|k| Change::new("variable", "removed", k.as_str())),
        )
        .collect();
    activity::record(
        &mut tx,
        scope.environment,
        scope.service,
        Some(actor.user_id),
        &changes,
    )
    .await?;
    let (target, target_id) = match scope.instance {
        Some(i) => ("service_instance", i),
        None => ("environment", scope.environment),
    };
    Entry::new("variables.update", meta)
        .actor(actor)
        .workspace(grant.workspace_id()?)
        .target(target, target_id)
        .metadata(json!({ "set": req.set.keys().collect::<Vec<_>>(), "deleted": deleted }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    list(state, scope).await
}

/// Every variable's references in the environment, as edges between keys.
async fn reference_edges(
    state: &AppState,
    environment: Id,
) -> AppResult<BTreeMap<VarKey, Vec<VarKey>>> {
    let rows: Vec<(Option<String>, String, Vec<String>)> = sqlx::query_as(
        "SELECT s.name, v.name, v.refs FROM variables v
         LEFT JOIN service_instances i ON i.id = v.service_instance_id
         LEFT JOIN services s ON s.id = i.service_id
         WHERE v.environment_id = $1",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(svc, name, keys)| {
            let targets = keys
                .iter()
                .filter_map(|k| Ref::parse(k))
                .map(|r| refs::resolve_key(svc.as_deref(), &r))
                .collect();
            ((svc, name), targets)
        })
        .collect())
}

async fn check_cycles(
    state: &AppState,
    scope: Scope,
    set: &BTreeMap<&String, Vec<String>>,
    delete: &BTreeSet<String>,
) -> AppResult<()> {
    if set.values().all(Vec::is_empty) {
        return Ok(()); // nothing new points anywhere
    }
    let owner: Option<String> = match scope.service {
        Some(s) => Some(
            sqlx::query_scalar("SELECT name FROM services WHERE id = $1")
                .bind(s)
                .fetch_one(&state.db)
                .await?,
        ),
        None => None,
    };
    let mut edges = reference_edges(state, scope.environment).await?;
    for name in delete {
        edges.remove(&(owner.clone(), name.clone()));
    }
    for (name, keys) in set {
        let targets = keys
            .iter()
            .filter_map(|k| Ref::parse(k))
            .map(|r| refs::resolve_key(owner.as_deref(), &r))
            .collect();
        edges.insert((owner.clone(), (*name).clone()), targets);
    }
    refs::check_cycles(&edges).map_err(|e| AppError::bad_request(e.to_string()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariableNames {
    pub shared: Vec<String>,
    pub services: Vec<ServiceNames>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceNames {
    pub service_id: Id,
    pub service_name: String,
    pub names: Vec<String>,
    pub platform: Vec<&'static str>,
}

/// Every name a reference in this environment can point at (for autocomplete).
pub async fn names(State(state): State<AppState>, grant: Grant) -> AppResult<Json<VariableNames>> {
    let environment = grant.id();
    let shared = sqlx::query_scalar(
        "SELECT name FROM variables WHERE environment_id = $1 AND service_instance_id IS NULL ORDER BY name",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    let rows: Vec<(Id, String, Vec<String>)> = sqlx::query_as(
        "SELECT s.id, s.name, coalesce(array_agg(v.name ORDER BY v.name) FILTER (WHERE v.name IS NOT NULL), '{}')
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         LEFT JOIN variables v ON v.service_instance_id = i.id
         WHERE i.environment_id = $1
         GROUP BY s.id, s.name ORDER BY s.name",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(VariableNames {
        shared,
        services: rows
            .into_iter()
            .map(|(service_id, service_name, names)| ServiceNames {
                service_id,
                service_name,
                names,
                platform: PLATFORM.to_vec(),
            })
            .collect(),
    }))
}

pub async fn reveal(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let (name, sealed, value, env_id): (String, bool, Vec<u8>, Id) = sqlx::query_as(
        "SELECT name, sealed, value_sealed, environment_id FROM variables WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if sealed {
        return Err(AppError::Conflict(
            "sealed variables can't be revealed".into(),
        ));
    }
    let plain = env::open_variable(&state.keys, &state.db, env_id, id, &value).await?;
    Entry::new("variable.reveal", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("variable", id)
        .metadata(json!({ "name": name }))
        .record(&state.db)
        .await?;
    Ok(Json(
        json!({ "name": name, "value": plain.expose_secret() }),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEnv {
    /// Name → value, references expanded.
    pub variables: BTreeMap<String, String>,
    /// Sealed variables and those referencing one: never sent.
    pub withheld: Vec<String>,
    /// References to variables that don't exist (expanded to "").
    pub missing: Vec<String>,
}

/// `GET /api/service-instances/{instance_id}/run-env` — the instance's
/// variables with their values, for running a command locally
/// (`modula run`). Audited with the names returned.
pub async fn run_env(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<RunEnv>> {
    let instance = grant.id();
    let environment: Id =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let resolved = env::resolve_for_run(
        &state.keys,
        &state.db,
        state.cfg.tcp_proxy_host.as_deref(),
        environment,
        instance,
    )
    .await?;
    let names: Vec<&String> = resolved.vars.keys().collect();
    Entry::new("variable.run_env", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", instance)
        .metadata(json!({ "names": names, "withheld": resolved.withheld }))
        .record(&state.db)
        .await?;
    Ok(Json(RunEnv {
        variables: resolved
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.expose_secret().to_owned()))
            .collect(),
        withheld: resolved.withheld,
        missing: resolved.missing,
    }))
}

/// A variable sealed and ready to insert (sealing happens outside the
/// transaction because the keyring may create the environment's key).
pub struct SealedVariable {
    id: Id,
    name: String,
    value: Vec<u8>,
    refs: Vec<String>,
}

pub async fn seal_new(
    state: &AppState,
    environment: Id,
    vars: &[(String, String)],
) -> AppResult<Vec<SealedVariable>> {
    let mut out = Vec::with_capacity(vars.len());
    for (name, value) in vars {
        env::validate_name(name)?;
        env::validate_value(value)?;
        let id = Id::new();
        out.push(SealedVariable {
            id,
            name: name.clone(),
            value: env::seal_variable(&state.keys, &state.db, environment, id, value).await?,
            refs: refs::references(value).iter().map(Ref::key).collect(),
        });
    }
    Ok(out)
}

/// Insert the variables of a new service instance (no cycle check needed:
/// nothing references a service that didn't exist).
pub async fn insert_new(
    tx: &mut sqlx::PgConnection,
    environment: Id,
    instance: Id,
    vars: &[SealedVariable],
) -> AppResult<()> {
    for v in vars {
        sqlx::query(
            "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, refs)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(v.id)
        .bind(environment)
        .bind(instance)
        .bind(&v.name)
        .bind(&v.value)
        .bind(&v.refs)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
