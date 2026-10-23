//! Services, their per-environment configuration (service instances) and
//! variables. Variable values are write-only through this API except for an
//! explicit, audited, sudo-gated reveal.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::env;
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

pub fn validate_service_name(name: &str) -> AppResult<()> {
    let b = name.as_bytes();
    let ok = (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if !ok {
        return Err(AppError::bad_request(
            "service names use lowercase letters, digits and -, start with a letter and are at most 63 characters",
        ));
    }
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: Uuid,
    pub service_id: Uuid,
    pub service_name: String,
    pub environment_id: Uuid,
    pub environment_name: String,
    pub project_id: Uuid,
    pub node_id: Option<Uuid>,
    #[sqlx(json)]
    pub config: ServiceConfig,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn load_instance(state: &AppState, id: Uuid) -> AppResult<Instance> {
    sqlx::query_as(
        "SELECT i.id, i.service_id, s.name AS service_name, i.environment_id, e.name AS environment_name,
                s.project_id, i.node_id, i.config, i.updated_at
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateService {
    pub name: String,
    pub config: ServiceConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedService {
    pub id: Uuid,
    pub name: String,
    pub instances: Vec<Instance>,
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateService>,
) -> AppResult<Json<CreatedService>> {
    let project = grant.id();
    validate_service_name(&req.name)?;
    req.config.validate()?;
    let config = serde_json::to_value(&req.config).map_err(anyhow::Error::from)?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO services (id, project_id, name) VALUES ($1, $2, $3)
         ON CONFLICT (project_id, name) DO NOTHING",
    )
    .bind(id)
    .bind(project)
    .bind(&req.name)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "a service named {} already exists",
            req.name
        )));
    }
    let instance_ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO service_instances (id, service_id, environment_id, config)
         SELECT gen_random_uuid(), $1, e.id, $3 FROM environments e WHERE e.project_id = $2
         RETURNING id",
    )
    .bind(id)
    .bind(project)
    .bind(&config)
    .fetch_all(&mut *tx)
    .await?;
    Entry::new("service.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service", id)
        .metadata(json!({ "project": project, "name": req.name, "image": req.config.image() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut instances = Vec::with_capacity(instance_ids.len());
    for i in instance_ids {
        instances.push(load_instance(&state, i).await?);
    }
    Ok(Json(CreatedService {
        id,
        name: req.name,
        instances,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let nodes = deploy::remove_live(&mut tx, LiveFilter::Service, id).await?;
    sqlx::query("DELETE FROM services WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("service.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(json!({ "ok": true })))
}

pub async fn get_instance(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Instance>> {
    load_instance(&state, grant.id()).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInstance {
    pub config: ServiceConfig,
}

/// Settings changes are recorded as a change set. Until staged changes ship
/// (C3) they are applied immediately; they take effect on the next deploy.
pub async fn update_instance(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateInstance>,
) -> AppResult<Json<Instance>> {
    let id = grant.id();
    req.config.validate()?;
    let config = serde_json::to_value(&req.config).map_err(anyhow::Error::from)?;
    let mut tx = state.db.begin().await?;
    let env: Uuid = sqlx::query_scalar(
        "UPDATE service_instances SET config = $2, updated_at = now() WHERE id = $1 RETURNING environment_id",
    )
    .bind(id)
    .bind(&config)
    .fetch_one(&mut *tx)
    .await?;
    record_change(
        &mut tx,
        env,
        id,
        "config",
        json!({ "image": req.config.image() }),
        actor.user_id,
    )
    .await?;
    Entry::new("service_instance.update", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", id)
        .metadata(json!({ "image": req.config.image() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    load_instance(&state, id).await.map(Json)
}

async fn record_change(
    tx: &mut sqlx::PgConnection,
    env: Uuid,
    instance: Uuid,
    kind: &str,
    summary: serde_json::Value,
    user: Uuid,
) -> AppResult<()> {
    let set = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO change_sets (id, environment_id, status, created_by, applied_at, applied_by)
         VALUES ($1, $2, 'applied', $3, now(), $3)",
    )
    .bind(set)
    .bind(env)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO changes (change_set_id, service_instance_id, kind, summary) VALUES ($1, $2, $3, $4)")
        .bind(set)
        .bind(instance)
        .bind(kind)
        .bind(summary)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct VariableInfo {
    pub id: Uuid,
    pub name: String,
    pub sealed: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

async fn list_variables(state: &AppState, instance: Uuid) -> AppResult<Vec<VariableInfo>> {
    Ok(sqlx::query_as(
        "SELECT id, name, sealed, updated_at FROM variables WHERE service_instance_id = $1 ORDER BY name",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await?)
}

/// Names and metadata only; values are never returned here.
pub async fn variables(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<VariableInfo>>> {
    list_variables(&state, grant.id()).await.map(Json)
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

pub async fn update_variables(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateVariables>,
) -> AppResult<Json<Vec<VariableInfo>>> {
    let instance = grant.id();
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
    let env_id: Uuid =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let existing: BTreeMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT name, id FROM variables WHERE service_instance_id = $1",
    )
    .bind(instance)
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
            "a service can have at most {} variables",
            env::MAX_VARIABLES
        )));
    }
    // Seal before the transaction (the keyring may need to create the
    // environment's data key). The row id is bound into each ciphertext.
    let mut rows = Vec::with_capacity(req.set.len());
    for (name, value) in &req.set {
        let id = existing.get(name).copied().unwrap_or_else(Uuid::now_v7);
        let sealed = env::seal_variable(&state.keys, &state.db, env_id, id, value).await?;
        rows.push((id, name, sealed, req.sealed.contains(name)));
    }
    let mut tx = state.db.begin().await?;
    for (id, name, sealed, is_sealed) in &rows {
        // Sealed is permanent. The id check guards against a concurrent insert
        // of the same name under a different id (its AAD would not match).
        let r = sqlx::query(
            "INSERT INTO variables (id, service_instance_id, name, value_sealed, sealed)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (service_instance_id, name) DO UPDATE
               SET value_sealed = EXCLUDED.value_sealed, sealed = variables.sealed OR EXCLUDED.sealed,
                   updated_at = now()
               WHERE variables.id = EXCLUDED.id",
        )
        .bind(id)
        .bind(instance)
        .bind(name)
        .bind(sealed)
        .bind(is_sealed)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(AppError::Conflict(
                "variables changed concurrently; retry".into(),
            ));
        }
    }
    let deleted: Vec<&String> = req.delete.iter().collect();
    sqlx::query("DELETE FROM variables WHERE service_instance_id = $1 AND name = ANY($2)")
        .bind(instance)
        .bind(&deleted)
        .execute(&mut *tx)
        .await?;
    let summary = json!({ "set": req.set.keys().collect::<Vec<_>>(), "deleted": deleted });
    record_change(
        &mut tx,
        env_id,
        instance,
        "variables",
        summary.clone(),
        actor.user_id,
    )
    .await?;
    Entry::new("variables.update", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", instance)
        .metadata(summary)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    list_variables(&state, instance).await.map(Json)
}

pub async fn reveal_variable(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let (name, sealed, value, env_id): (String, bool, Vec<u8>, Uuid) = sqlx::query_as(
        "SELECT v.name, v.sealed, v.value_sealed, i.environment_id
         FROM variables v JOIN service_instances i ON i.id = v.service_instance_id WHERE v.id = $1",
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
