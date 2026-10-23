//! Services and their per-environment configuration (service instances).
//! Variables are in [`super::variables`].

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::activity::{self, Change};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
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
    pub id: Id,
    pub service_id: Id,
    pub service_name: String,
    pub environment_id: Id,
    pub environment_name: String,
    pub project_id: Id,
    pub node_id: Option<Id>,
    #[sqlx(json)]
    pub config: ServiceConfig,
    /// `online`, `deploying`, `crashed` or `offline`, from its deployments.
    pub status: String,
    #[serde(skip)]
    pub location_id: Option<String>,
    /// Where the server running it is.
    #[sqlx(skip)]
    pub location: Option<&'static crate::locations::Location>,
    pub stack: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn load_instance(state: &AppState, id: Id) -> AppResult<Instance> {
    let mut i: Instance = sqlx::query_as(
        "SELECT i.id, i.service_id, s.name AS service_name, i.environment_id, e.name AS environment_name,
                s.project_id, i.node_id, i.config, i.stack, i.updated_at, n.location AS location_id,
                CASE
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id AND status = 'active')
                    THEN 'online'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id
                               AND status IN ('queued', 'building', 'deploying'))
                    THEN 'deploying'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id AND status = 'crashed')
                    THEN 'crashed'
                  ELSE 'offline'
                END AS status
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         LEFT JOIN nodes n ON n.id = i.node_id
         WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    i.location = i.location_id.as_deref().and_then(crate::locations::get);
    Ok(i)
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
    pub id: Id,
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
    let id = Id::new();
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
    let instance_ids: Vec<Id> = sqlx::query_scalar(
        "INSERT INTO service_instances (id, service_id, environment_id, config)
         SELECT new_id(), $1, e.id, $3 FROM environments e WHERE e.project_id = $2
         RETURNING id",
    )
    .bind(id)
    .bind(project)
    .bind(&config)
    .fetch_all(&mut *tx)
    .await?;
    let envs: Vec<Id> = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    for env in envs {
        let created = [Change::new("service", "created", req.name.as_str())];
        activity::record(&mut tx, env, Some(id), Some(actor.user_id), &created).await?;
    }
    Entry::new("service.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service", id)
        .metadata(
            json!({ "project": project, "name": req.name, "source": req.config.source_label() }),
        )
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
    let (env, service, old): (Id, Id, serde_json::Value) = sqlx::query_as(
        "SELECT environment_id, service_id, config FROM service_instances WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE service_instances SET config = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(&config)
        .execute(&mut *tx)
        .await?;
    let changes = activity::config_changes(&old, &config);
    activity::record(&mut tx, env, Some(service), Some(actor.user_id), &changes).await?;
    Entry::new("service_instance.update", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", id)
        .metadata(json!({ "source": req.config.source_label() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    load_instance(&state, id).await.map(Json)
}
