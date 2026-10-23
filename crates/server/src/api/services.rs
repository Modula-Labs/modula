//! Services and their per-environment configuration (service instances).
//! Variables are in [`super::variables`].

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::activity::{self, Change};
use crate::api::variables;
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::templates;

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
    /// The database template the service was created from.
    pub template: Option<String>,
    #[sqlx(json(nullable))]
    pub volume: Option<serde_json::Value>,
    #[sqlx(json(nullable))]
    pub tcp_proxy: Option<serde_json::Value>,
    #[serde(skip)]
    pub tcp_node_ip: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn load_instance(state: &AppState, id: Id) -> AppResult<Instance> {
    let mut i: Instance = sqlx::query_as(
        "SELECT i.id, i.service_id, s.name AS service_name, i.environment_id, e.name AS environment_name,
                s.project_id, i.node_id, i.config, i.stack, i.updated_at, n.location AS location_id,
                s.template,
                (SELECT json_build_object('id', v.id, 'mountPath', v.mount_path, 'createdAt', v.created_at)
                 FROM volumes v WHERE v.service_instance_id = i.id) AS volume,
                (SELECT json_build_object('id', t.id, 'publicPort', t.public_port, 'targetPort', t.target_port)
                 FROM tcp_proxies t WHERE t.service_instance_id = i.id) AS tcp_proxy,
                (SELECT tn.public_ip FROM tcp_proxies t JOIN nodes tn ON tn.id = t.node_id
                 WHERE t.service_instance_id = i.id) AS tcp_node_ip,
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
    if let Some(p) = i.tcp_proxy.as_mut() {
        let host = state
            .cfg
            .tcp_proxy_host
            .clone()
            .or(i.tcp_node_ip.clone())
            .unwrap_or_default();
        p["host"] = serde_json::Value::String(host);
    }
    Ok(i)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateService {
    pub name: String,
    /// Required unless `template` is given.
    pub config: Option<ServiceConfig>,
    /// A database template id (see `templates.rs`).
    pub template: Option<String>,
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
    let template = match req.template.as_deref() {
        Some(t) => {
            Some(templates::get(t).ok_or_else(|| AppError::bad_request("unknown template"))?)
        }
        None => None,
    };
    let cfg = match (&req.config, template) {
        (Some(c), _) => c.clone(),
        (None, Some(t)) => t.config(),
        (None, None) => return Err(AppError::bad_request("config or template is required")),
    };
    cfg.validate_for_instance()?;
    let config = serde_json::to_value(&cfg).map_err(anyhow::Error::from)?;
    let id = Id::new();

    // One instance per environment; a template's variables (with their own
    // passwords) are sealed per environment before the transaction.
    let envs: Vec<Id> =
        sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1 ORDER BY created_at")
            .bind(project)
            .fetch_all(&state.db)
            .await?;
    let mut instances = Vec::with_capacity(envs.len());
    for env in envs {
        let vars = match template {
            Some(t) => variables::seal_new(&state, env, &t.variables()).await?,
            None => Vec::new(),
        };
        instances.push((env, Id::new(), vars));
    }

    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO services (id, project_id, name, template) VALUES ($1, $2, $3, $4)
         ON CONFLICT (project_id, name) DO NOTHING",
    )
    .bind(id)
    .bind(project)
    .bind(&req.name)
    .bind(template.map(|t| t.id))
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "a service named {} already exists",
            req.name
        )));
    }
    for (env, instance, vars) in &instances {
        sqlx::query(
            "INSERT INTO service_instances (id, service_id, environment_id, config) VALUES ($1, $2, $3, $4)",
        )
        .bind(instance)
        .bind(id)
        .bind(env)
        .bind(&config)
        .execute(&mut *tx)
        .await?;
        variables::insert_new(&mut tx, *env, *instance, vars).await?;
        if let Some(t) = template {
            sqlx::query(
                "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, $3)",
            )
            .bind(Id::new())
            .bind(instance)
            .bind(t.mount_path)
            .execute(&mut *tx)
            .await?;
        }
        let created = [Change::new("service", "created", req.name.as_str())];
        activity::record(&mut tx, *env, Some(id), Some(actor.user_id), &created).await?;
    }
    Entry::new("service.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service", id)
        .metadata(json!({
            "project": project,
            "name": req.name,
            "source": cfg.source_label(),
            "template": template.map(|t| t.id),
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut loaded = Vec::with_capacity(instances.len());
    for (_, i, _) in instances {
        loaded.push(load_instance(&state, i).await?);
    }
    Ok(Json(CreatedService {
        id,
        name: req.name,
        instances: loaded,
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
    let mut nodes = deploy::remove_live(&mut tx, LiveFilter::Service, id).await?;
    nodes.extend(deploy::queue_volume_removals(&mut tx, LiveFilter::Service, id).await?);
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
    set_config(
        &state,
        &actor,
        grant.workspace_id()?,
        &meta,
        id,
        &req.config,
    )
    .await?;
    load_instance(&state, id).await.map(Json)
}

/// Replace an instance's settings, recording which fields changed.
pub(crate) async fn set_config(
    state: &AppState,
    actor: &Actor,
    workspace: Id,
    meta: &RequestMeta,
    id: Id,
    new: &ServiceConfig,
) -> AppResult<()> {
    new.validate_for_instance()?;
    let config = serde_json::to_value(new).map_err(anyhow::Error::from)?;
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
    Entry::new("service_instance.update", meta)
        .actor(actor)
        .workspace(workspace)
        .target("service_instance", id)
        .metadata(json!({ "source": new.source_label() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
