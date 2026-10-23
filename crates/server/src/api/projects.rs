//! Projects and environments. Every project starts with a `production`
//! environment; services exist in every environment of their project.

use std::collections::BTreeMap;

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
use crate::deploy::refs::{self, Ref};
use crate::deploy::{self, LiveFilter, env};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::templates;
use crate::validate;
use secrecy::ExposeSecret;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: Id,
    pub workspace_id: Id,
    pub name: String,
    pub description: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: Id,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSummary {
    pub id: Id,
    pub service_id: Id,
    pub environment_id: Id,
    pub node_id: Option<Id>,
    pub latest_deployment_id: Option<Id>,
    pub latest_deployment_status: Option<String>,
    /// The first public hostname, shown on the canvas card.
    pub domain: Option<String>,
    /// `image` or `repo`, and the repository for repo sources (card icon/subtitle).
    pub source_type: Option<String>,
    pub repo: Option<String>,
    pub image: Option<String>,
    /// Detected by the last build (e.g. `nextjs`), for the card icon.
    pub stack: Option<String>,
    /// Services this one references (`${{service.NAME}}`, directly or through
    /// a shared variable): the canvas draws an edge to each.
    pub depends_on: Vec<Id>,
    pub template: Option<String>,
    #[sqlx(json(nullable))]
    pub volume: Option<serde_json::Value>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub template: Option<String>,
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
    let id = Id::new();
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
        .bind(Id::new())
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

pub async fn detail(state: &AppState, id: Id) -> AppResult<ProjectDetail> {
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
        "SELECT id, name, icon, template FROM services WHERE project_id = $1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let instances = sqlx::query_as(
        "SELECT i.id, i.service_id, i.environment_id, i.node_id,
                d.id AS latest_deployment_id, d.status AS latest_deployment_status,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id AND status = 'verified'
                 ORDER BY created_at LIMIT 1) AS domain,
                i.config->'source'->>'type' AS source_type,
                i.config->'source'->>'repo' AS repo,
                i.config->'source'->>'image' AS image,
                i.stack,
                ARRAY(
                    WITH own AS (
                        SELECT unnest(refs) AS r FROM variables WHERE service_instance_id = i.id
                    ), via_shared AS (
                        SELECT unnest(v.refs) AS r FROM variables v
                        WHERE v.environment_id = i.environment_id AND v.service_instance_id IS NULL
                          AND 'shared.' || v.name IN (SELECT r FROM own)
                    )
                    SELECT DISTINCT d.id::text FROM (SELECT r FROM own UNION SELECT r FROM via_shared) x
                    JOIN services d ON d.project_id = s.project_id AND d.id <> s.id
                                   AND d.name = split_part(x.r, '.', 1)
                    WHERE position('.' in x.r) > 0 AND x.r NOT LIKE 'shared.%'
                ) AS depends_on,
                s.template,
                (SELECT json_build_object('id', v.id, 'mountPath', v.mount_path)
                 FROM volumes v WHERE v.service_instance_id = i.id) AS volume
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
async fn forget_environments(state: &AppState, envs: &[Id]) {
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
    let mut nodes = deploy::remove_live(&mut tx, LiveFilter::Project, id).await?;
    nodes.extend(deploy::queue_volume_removals(&mut tx, LiveFilter::Project, id).await?);
    let envs: Vec<Id> = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateEnvironment {
    pub name: String,
    /// The environment to copy service configs from (default: the first).
    #[serde(default)]
    pub clone_from: Option<Id>,
    /// Also copy its variables (sealed ones excepted).
    #[serde(default)]
    pub copy_variables: bool,
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
    let id = Id::new();
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
    // the configuration of another (by default the project's first).
    let source: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM environments WHERE project_id = $1 AND id <> $2 AND ($3::text IS NULL OR id = $3)
         ORDER BY created_at LIMIT 1",
    )
    .bind(project)
    .bind(id)
    .bind(req.clone_from)
    .fetch_optional(&mut *tx)
    .await?;
    if req.clone_from.is_some() && source.is_none() {
        return Err(AppError::bad_request(
            "cloneFrom must be an environment of this project",
        ));
    }
    sqlx::query(
        "INSERT INTO service_instances (id, service_id, environment_id, config)
         SELECT new_id(), i.service_id, $2, i.config FROM service_instances i WHERE i.environment_id = $1",
    )
    .bind(source)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // Volume definitions too (a fresh, empty volume each, never the data), so
    // databases keep their persistence. Domains and TCP proxies are not
    // copied: public exposure is always explicit.
    sqlx::query(
        "INSERT INTO volumes (id, service_instance_id, mount_path)
         SELECT new_id(), ti.id, v.mount_path FROM volumes v
         JOIN service_instances si ON si.id = v.service_instance_id
         JOIN service_instances ti ON ti.service_id = si.service_id AND ti.environment_id = $2
         WHERE si.environment_id = $1",
    )
    .bind(source)
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
    // Fresh template passwords go in first so a variable copy can't bring
    // the source's; the template's other variables fill whatever is still
    // missing afterwards.
    seed_template_variables(&state, id, true).await?;
    if let (true, Some(from)) = (req.copy_variables, source) {
        copy_environment(&state, actor.user_id, id, from).await?;
    }
    seed_template_variables(&state, id, false).await?;
    Ok(Json(
        sqlx::query_as("SELECT id, name, created_at FROM environments WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?,
    ))
}

/// Give the template (database) services of a new environment the
/// variables their template creates, with freshly generated passwords: sealed
/// values are never copied, and a clone shouldn't share credentials with its
/// source anyway. `secrets_only` adds just the passwords. Existing variables
/// are kept.
async fn seed_template_variables(
    state: &AppState,
    environment: Id,
    secrets_only: bool,
) -> AppResult<()> {
    let instances: Vec<(Id, String)> = sqlx::query_as(
        "SELECT i.id, s.template FROM service_instances i
         JOIN services s ON s.id = i.service_id
         WHERE i.environment_id = $1 AND s.template IS NOT NULL",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    // Seal before the transaction (the keyring may create the data key).
    let mut rows = Vec::new();
    for (instance, template) in instances {
        let Some(t) = templates::get(&template) else {
            continue;
        };
        for ((name, default), (_, value)) in t.variables.iter().zip(t.variables()) {
            if secrets_only && *default != templates::SECRET {
                continue;
            }
            let id = Id::new();
            let sealed =
                env::seal_variable(&state.keys, &state.db, environment, id, &value).await?;
            let refs: Vec<String> = refs::references(&value).iter().map(Ref::key).collect();
            rows.push((id, instance, *name, sealed, refs));
        }
    }
    let mut tx = state.db.begin().await?;
    for (id, instance, name, sealed, refs) in &rows {
        sqlx::query(
            "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, refs)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (environment_id, service_instance_id, name) DO NOTHING",
        )
        .bind(id)
        .bind(environment)
        .bind(instance)
        .bind(name)
        .bind(sealed)
        .bind(refs)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
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
    let mut nodes = deploy::remove_live(&mut tx, LiveFilter::Environment, id).await?;
    nodes.extend(deploy::queue_volume_removals(&mut tx, LiveFilter::Environment, id).await?);
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncEnvironment {
    pub from_environment_id: Id,
}

/// Bring an environment in line with another of the same project: every
/// service's config is copied, and variables the target lacks are added.
/// Existing target variables keep their values; sealed ones are never copied.
/// Takes effect on the next deploy of each service, like any config change.
pub async fn sync_environment(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<SyncEnvironment>,
) -> AppResult<Json<serde_json::Value>> {
    let target = grant.id();
    let from = req.from_environment_id;
    let same_project: bool = sqlx::query_scalar(
        "SELECT count(*) = 2 AND count(DISTINCT project_id) = 1 FROM environments WHERE id IN ($1, $2)",
    )
    .bind(target)
    .bind(from)
    .fetch_one(&state.db)
    .await?;
    if from == target || !same_project {
        return Err(AppError::bad_request(
            "pick another environment of the same project",
        ));
    }
    let (created, updated) = copy_environment(&state, actor.user_id, target, from).await?;
    Entry::new("environment.sync", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("environment", target)
        .metadata(json!({ "from": from, "updatedServices": updated, "addedVariables": created }))
        .record(&state.db)
        .await?;
    Ok(Json(json!({ "created": created, "updated": updated })))
}

/// Copy `from`'s service configs into `target` and add the (non-sealed)
/// variables `target` lacks; returns (variables added, services updated).
/// Both environments must belong to the same project.
async fn copy_environment(
    state: &AppState,
    user: Id,
    target: Id,
    from: Id,
) -> AppResult<(u32, u32)> {
    // Copy variables first (sealing needs the keyring, outside the transaction).
    // (target instance or None for shared, source variable, name, value, refs)
    #[allow(clippy::type_complexity)]
    let missing: Vec<(Option<Id>, Id, String, Vec<u8>, Vec<String>)> = sqlx::query_as(
        "SELECT ti.id, v.id, v.name, v.value_sealed, v.refs
         FROM variables v
         LEFT JOIN service_instances si ON si.id = v.service_instance_id
         LEFT JOIN service_instances ti ON ti.service_id = si.service_id AND ti.environment_id = $1
         WHERE v.environment_id = $2 AND NOT v.sealed
           AND (v.service_instance_id IS NULL OR ti.id IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM variables t WHERE t.environment_id = $1
                           AND t.service_instance_id IS NOT DISTINCT FROM ti.id AND t.name = v.name)",
    )
    .bind(target)
    .bind(from)
    .fetch_all(&state.db)
    .await?;
    let mut copies = Vec::with_capacity(missing.len());
    for (instance, var, name, sealed, refs) in missing {
        let value = env::open_variable(&state.keys, &state.db, from, var, &sealed).await?;
        let id = Id::new();
        let resealed =
            env::seal_variable(&state.keys, &state.db, target, id, value.expose_secret()).await?;
        copies.push((instance, id, name, resealed, refs));
    }

    let mut tx = state.db.begin().await?;
    let pairs: Vec<(Id, Id, serde_json::Value, serde_json::Value)> = sqlx::query_as(
        "SELECT ti.id, ti.service_id, ti.config, si.config
         FROM service_instances ti
         JOIN service_instances si ON si.service_id = ti.service_id AND si.environment_id = $2
         WHERE ti.environment_id = $1
         FOR UPDATE OF ti",
    )
    .bind(target)
    .bind(from)
    .fetch_all(&mut *tx)
    .await?;
    let mut changes: BTreeMap<Option<Id>, Vec<Change>> = BTreeMap::new();
    let mut updated = 0;
    for (instance, service, old, new) in pairs {
        let diff = activity::config_changes(&old, &new);
        if diff.is_empty() {
            continue;
        }
        sqlx::query("UPDATE service_instances SET config = $2, updated_at = now() WHERE id = $1")
            .bind(instance)
            .bind(&new)
            .execute(&mut *tx)
            .await?;
        updated += 1;
        changes.entry(Some(service)).or_default().extend(diff);
    }
    let mut created = 0;
    for (instance, id, name, sealed, refs) in &copies {
        let r = sqlx::query(
            "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, refs)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (environment_id, service_instance_id, name) DO NOTHING",
        )
        .bind(id)
        .bind(target)
        .bind(instance)
        .bind(name)
        .bind(sealed)
        .bind(refs)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 1 {
            created += 1;
            let service: Option<Id> = match instance {
                Some(i) => Some(
                    sqlx::query_scalar("SELECT service_id FROM service_instances WHERE id = $1")
                        .bind(i)
                        .fetch_one(&mut *tx)
                        .await?,
                ),
                None => None,
            };
            changes.entry(service).or_default().push(Change::new(
                "variable",
                "added",
                name.as_str(),
            ));
        }
    }
    for (service, list) in &changes {
        activity::record(&mut tx, target, *service, Some(user), list).await?;
    }
    tx.commit().await?;
    Ok((created, updated))
}
