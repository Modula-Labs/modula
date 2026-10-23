//! Projects and environments. Every project starts with a `production`
//! environment; services exist in every environment of their project.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::validate;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub description: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: Uuid,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSummary {
    pub id: Uuid,
    pub service_id: Uuid,
    pub environment_id: Uuid,
    pub node_id: Option<Uuid>,
    pub latest_deployment_id: Option<Uuid>,
    pub latest_deployment_status: Option<String>,
    /// The first public hostname, shown on the canvas card.
    pub domain: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub id: Uuid,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub project: Project,
    pub environments: Vec<Environment>,
    pub services: Vec<Service>,
    pub instances: Vec<InstanceSummary>,
}

const PRODUCTION: &str = "production";

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Project>>> {
    let ws = grant.workspace_id()?;
    Ok(Json(
        sqlx::query_as(
            "SELECT id, workspace_id, name, description, created_at FROM projects
             WHERE workspace_id = $1 ORDER BY created_at",
        )
        .bind(ws)
        .fetch_all(&state.db)
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

fn description(raw: &str) -> AppResult<String> {
    let d = raw.trim();
    if d.chars().count() > 500 || d.chars().any(|c| c.is_control() && c != '\n') {
        return Err(AppError::bad_request(
            "description must be under 500 characters",
        ));
    }
    Ok(d.to_owned())
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateProject>,
) -> AppResult<Json<ProjectDetail>> {
    let ws = grant.workspace_id()?;
    let name = validate::display_name(&req.name, "project name")?;
    let desc = description(&req.description)?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO projects (id, workspace_id, name, description) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(ws)
    .bind(&name)
    .bind(&desc)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO environments (id, project_id, name) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(PRODUCTION)
        .execute(&mut *tx)
        .await?;
    Entry::new("project.create", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("project", id)
        .metadata(json!({ "name": name }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    detail(&state, id).await.map(Json)
}

pub async fn detail(state: &AppState, id: Uuid) -> AppResult<ProjectDetail> {
    let project: Project = sqlx::query_as(
        "SELECT id, workspace_id, name, description, created_at FROM projects WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let environments = sqlx::query_as(
        "SELECT id, name, created_at FROM environments WHERE project_id = $1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let services = sqlx::query_as(
        "SELECT id, name, icon FROM services WHERE project_id = $1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let instances = sqlx::query_as(
        "SELECT i.id, i.service_id, i.environment_id, i.node_id,
                d.id AS latest_deployment_id, d.status AS latest_deployment_status,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id
                 ORDER BY created_at LIMIT 1) AS domain
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         LEFT JOIN LATERAL (
             SELECT id, status FROM deployments WHERE service_instance_id = i.id
             ORDER BY created_at DESC LIMIT 1
         ) d ON true
         WHERE s.project_id = $1
         ORDER BY s.created_at, i.created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(ProjectDetail {
        project,
        environments,
        services,
        instances,
    })
}

pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<ProjectDetail>> {
    detail(&state, grant.id()).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProject {
    pub name: Option<String>,
    pub description: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateProject>,
) -> AppResult<Json<ProjectDetail>> {
    let id = grant.id();
    let name = req
        .name
        .as_deref()
        .map(|n| validate::display_name(n, "project name"))
        .transpose()?;
    let desc = req.description.as_deref().map(description).transpose()?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE projects SET name = COALESCE($2, name), description = COALESCE($3, description),
                updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(&name)
    .bind(&desc)
    .execute(&mut *tx)
    .await?;
    Entry::new("project.update", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("project", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    detail(&state, id).await.map(Json)
}

/// Remove data keys of deleted environments, after commit.
async fn forget_environments(state: &AppState, envs: &[Uuid]) {
    for e in envs {
        state.keys.forget(*e);
    }
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let nodes = deploy::remove_live(&mut tx, LiveFilter::Project, id).await?;
    let envs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM data_keys WHERE scope = ANY($1)")
        .bind(&envs)
        .execute(&mut *tx)
        .await?;
    Entry::new("project.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("project", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    forget_environments(&state, &envs).await;
    state.hub.kick_all(&nodes);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEnvironment {
    pub name: String,
}

pub async fn create_environment(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateEnvironment>,
) -> AppResult<Json<Environment>> {
    let project = grant.id();
    let name = validate::display_name(&req.name, "environment name")?.to_lowercase();
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO environments (id, project_id, name) VALUES ($1, $2, $3)
         ON CONFLICT (project_id, name) DO NOTHING",
    )
    .bind(id)
    .bind(project)
    .bind(&name)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "an environment named {name} already exists"
        )));
    }
    // Every service exists in every environment. New environments start from
    // the configuration of the project's first environment (not its variables).
    sqlx::query(
        "INSERT INTO service_instances (id, service_id, environment_id, config)
         SELECT gen_random_uuid(), i.service_id, $2, i.config
         FROM service_instances i
         WHERE i.environment_id = (
             SELECT id FROM environments WHERE project_id = $1 AND id <> $2 ORDER BY created_at LIMIT 1
         )",
    )
    .bind(project)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    Entry::new("environment.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("environment", id)
        .metadata(json!({ "project": project, "name": name }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(
        sqlx::query_as("SELECT id, name, created_at FROM environments WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?,
    ))
}

pub async fn delete_environment(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM environments
         WHERE project_id = (SELECT project_id FROM environments WHERE id = $1) AND id <> $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if others == 0 {
        return Err(AppError::Conflict(
            "a project needs at least one environment".into(),
        ));
    }
    let nodes = deploy::remove_live(&mut tx, LiveFilter::Environment, id).await?;
    sqlx::query("DELETE FROM environments WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM data_keys WHERE scope = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("environment.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("environment", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    forget_environments(&state, &[id]).await;
    state.hub.kick_all(&nodes);
    Ok(Json(json!({ "ok": true })))
}
