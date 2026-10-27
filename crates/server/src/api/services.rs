//! Services and their per-environment configuration (service instances).
//! Variables are in [`super::variables`].

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::activity::{self, Change};
use crate::api::{buckets, variables};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::env;
use crate::deploy::refs::{self, Ref};
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::mesh;
use crate::state::AppState;
use crate::templates;

/// A service name is a DNS label (`<name>.modula.internal`) and the prefix
/// of references to its variables (`${{name.VAR}}`, so never `shared`).
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
    if name == "shared" {
        return Err(AppError::bad_request(
            "\"shared\" is reserved for shared variables (${{shared.NAME}})",
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
    /// `online`, `degraded` (a replica of the serving deployment isn't running),
    /// `sleeping` (serverless, scaled to zero), `deploying`, `crashed` or
    /// `offline`, from its deployments.
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
    /// An R2 bucket service's bucket in this environment (None = none yet).
    #[sqlx(json(nullable))]
    pub r2: Option<serde_json::Value>,
    #[serde(skip)]
    pub tcp_node_ip: Option<String>,
    /// Edge rule actions ("rateLimit", "cache") a server serving its live
    /// deployment can't apply (its Caddy lacks the module).
    pub edge_unsupported: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn load_instance(state: &AppState, id: Id) -> AppResult<Instance> {
    let mut i: Instance = sqlx::query_as(concat!(
        "SELECT i.id, i.service_id, s.name AS service_name, i.environment_id, e.name AS environment_name,
                s.project_id, i.node_id, i.config, i.stack, i.updated_at, n.location AS location_id,
                s.template,
                (SELECT json_build_object('id', v.id, 'mountPath', v.mount_path, 'createdAt', v.created_at)
                 FROM volumes v WHERE v.service_instance_id = i.id) AS volume,
                (SELECT json_build_object('id', t.id, 'publicPort', t.public_port, 'targetPort', t.target_port)
                 FROM tcp_proxies t WHERE t.service_instance_id = i.id) AS tcp_proxy,
                (SELECT json_build_object('bucket', r.bucket, 'accountId', r.account_id, 'created', r.created,
                                          'createdAt', r.created_at)
                 FROM r2_buckets r WHERE r.service_instance_id = i.id) AS r2,
                (SELECT tn.public_ip FROM tcp_proxies t JOIN nodes tn ON tn.id = t.node_id
                 WHERE t.service_instance_id = i.id) AS tcp_node_ip,
                ARRAY(SELECT DISTINCT a FROM deployments d
                      JOIN deployment_replicas r ON r.deployment_id = d.id
                      JOIN nodes rn ON rn.id = r.node_id
                      CROSS JOIN unnest(rn.edge_unsupported) AS a
                      WHERE d.service_instance_id = i.id AND d.status IN ('active', 'sleeping', 'crashed')
                      ORDER BY a) AS edge_unsupported,
                CASE
                  WHEN EXISTS (SELECT 1 FROM deployments d WHERE d.service_instance_id = i.id AND ",
        crate::degraded_sql!(),
        ")
                    THEN 'degraded'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id AND status = 'active')
                    THEN 'online'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id AND status = 'sleeping')
                    THEN 'sleeping'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id
                               AND status IN ('waiting_ci', 'queued', 'building', 'deploying'))
                    THEN 'deploying'
                  WHEN EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = i.id AND status = 'crashed')
                    THEN 'crashed'
                  ELSE 'offline'
                END AS status
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         LEFT JOIN nodes n ON n.id = i.node_id
         WHERE i.id = $1"
    ))
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
    let workspace = grant.workspace_id()?;
    create_service(
        &state,
        &actor,
        &meta,
        workspace,
        grant.id(),
        req,
        &HashMap::new(),
    )
    .await
    .map(Json)
}

/// Create a service in `project` with one instance per environment.
/// `overrides` replaces (or adds) template variables of the instance in an
/// environment, by name: a restored or replicated database keeps the
/// credentials of the one it came from.
pub async fn create_service(
    state: &AppState,
    actor: &Actor,
    meta: &RequestMeta,
    workspace: Id,
    project: Id,
    req: CreateService,
    overrides: &HashMap<Id, Vec<(String, String)>>,
) -> AppResult<CreatedService> {
    validate_service_name(&req.name)?;
    let template = match req.template.as_deref() {
        Some(buckets::R2_TEMPLATE) => {
            return Err(AppError::bad_request(
                "R2 buckets need Cloudflare: add them from the dashboard (POST /api/projects/{id}/buckets)",
            ));
        }
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
    if !cfg.deployable() {
        return Err(AppError::bad_request(
            "R2 buckets are added with POST /api/projects/{id}/buckets",
        ));
    }
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
            Some(t) => {
                let mut vars = t.variables();
                for (name, value) in overrides.get(&env).into_iter().flatten() {
                    match vars.iter_mut().find(|(k, _)| k == name) {
                        Some(v) => v.1 = value.clone(),
                        None => vars.push((name.clone(), value.clone())),
                    }
                }
                variables::seal_new(state, env, &vars).await?
            }
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
    Entry::new("service.create", meta)
        .actor(actor)
        .workspace(workspace)
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
        loaded.push(load_instance(state, i).await?);
    }
    Ok(CreatedService {
        id,
        name: req.name,
        instances: loaded,
    })
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<buckets::Released>> {
    delete_service(&state, &actor, grant.workspace_id()?, &meta, grant.id())
        .await
        .map(Json)
}

/// Delete a service everywhere: its live deployments stop and its volumes
/// are removed.
pub(crate) async fn delete_service(
    state: &AppState,
    actor: &Actor,
    workspace: Id,
    meta: &RequestMeta,
    id: Id,
) -> AppResult<buckets::Released> {
    // An R2 bucket's tokens are revoked first; its buckets stay.
    let warnings = buckets::release(state, buckets::Scope::Service(id)).await?;
    let mut tx = state.db.begin().await?;
    let mut nodes = deploy::remove_live(&mut tx, LiveFilter::Service, id).await?;
    nodes.extend(deploy::queue_volume_removals(&mut tx, LiveFilter::Service, id).await?);
    sqlx::query("DELETE FROM services WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("service.delete", meta)
        .actor(actor)
        .workspace(workspace)
        .target("service", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(buckets::Released { ok: true, warnings })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateService {
    /// The new name; references to the service's variables follow it.
    pub name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamedService {
    pub id: Id,
    pub name: String,
    /// How many variables had references to the old name rewritten.
    pub rewritten_variables: usize,
}

/// A variable of the project (in any environment) and whose it is.
#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ProjectVariable {
    #[serde(skip)]
    pub id: Id,
    pub environment_id: Id,
    pub environment_name: String,
    #[serde(skip)]
    pub service_id: Option<Id>,
    /// None: a shared variable of the environment.
    pub service_name: Option<String>,
    pub name: String,
}

/// The variables of `project` with a `${{name.VAR}}` reference to `name`,
/// from the references stored in clear (nothing is decrypted).
macro_rules! referencing_sql {
    () => {
        "SELECT v.id, v.environment_id, e.name AS environment_name,
            s.id AS service_id, s.name AS service_name, v.name
     FROM variables v
     JOIN environments e ON e.id = v.environment_id
     LEFT JOIN service_instances i ON i.id = v.service_instance_id
     LEFT JOIN services s ON s.id = i.service_id
     WHERE e.project_id = $1
       AND EXISTS (SELECT 1 FROM unnest(v.refs) k WHERE starts_with(k, $2 || '.'))
     ORDER BY e.created_at, s.name NULLS FIRST, v.name"
    };
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePreview {
    pub name: String,
    /// Variables a rename rewrites (`${{name.VAR}}` → `${{new.VAR}}`).
    pub references: Vec<ProjectVariable>,
    /// Variables whose names contain the service's (`POSTGRES_1_URL` for
    /// `postgres-1`): a rename leaves them as they are.
    pub similar_names: Vec<ProjectVariable>,
}

/// `GET /api/services/{id}/rename-preview`: what renaming the service
/// changes, for the confirmation.
pub async fn rename_preview(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<RenamePreview>> {
    let (project, name): (Id, String) =
        sqlx::query_as("SELECT project_id, name FROM services WHERE id = $1")
            .bind(grant.id())
            .fetch_one(&state.db)
            .await?;
    let references = sqlx::query_as(referencing_sql!())
        .bind(project)
        .bind(&name)
        .fetch_all(&state.db)
        .await?;
    let similar_names = sqlx::query_as(
        "SELECT v.id, v.environment_id, e.name AS environment_name,
                s.id AS service_id, s.name AS service_name, v.name
         FROM variables v
         JOIN environments e ON e.id = v.environment_id
         LEFT JOIN service_instances i ON i.id = v.service_instance_id
         LEFT JOIN services s ON s.id = i.service_id
         WHERE e.project_id = $1 AND strpos(upper(v.name), upper(replace($2, '-', '_'))) > 0
         ORDER BY e.created_at, s.name NULLS FIRST, v.name",
    )
    .bind(project)
    .bind(&name)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(RenamePreview {
        name,
        references,
        similar_names,
    }))
}

/// Rename a service, like Railway: in one transaction, every
/// `${{old.VAR}}` in the project's variables (every environment, service
/// and shared ones) becomes `${{new.VAR}}`, and the services whose
/// variables changed have pending changes, as with any variable edit.
/// `<old>.modula.internal` keeps resolving in an environment while a
/// deployment from before the rename runs there (`service_aliases`).
pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateService>,
) -> AppResult<Json<RenamedService>> {
    let id = grant.id();
    let new = req.name;
    validate_service_name(&new)?;
    let mut tx = state.db.begin().await?;
    let (project, old): (Id, String) =
        sqlx::query_as("SELECT project_id, name FROM services WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if old == new {
        return Ok(Json(RenamedService {
            id,
            name: new,
            rewritten_variables: 0,
        }));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM services WHERE project_id = $1 AND name = $2)",
    )
    .bind(project)
    .bind(&new)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(AppError::Conflict(format!(
            "a service named {new} already exists"
        )));
    }

    // Rewrite the references (values are sealed per variable: open, rewrite,
    // seal again under the same row id). The environments' keys exist, so
    // the keyring reads them without touching the locked rows.
    let referencing: Vec<ProjectVariable> =
        sqlx::query_as(concat!(referencing_sql!(), " FOR UPDATE OF v"))
            .bind(project)
            .bind(&old)
            .fetch_all(&mut *tx)
            .await?;
    for v in &referencing {
        let sealed: Vec<u8> =
            sqlx::query_scalar("SELECT value_sealed FROM variables WHERE id = $1")
                .bind(v.id)
                .fetch_one(&mut *tx)
                .await?;
        let plain =
            env::open_variable(&state.keys, &state.db, v.environment_id, v.id, &sealed).await?;
        let value =
            refs::rename_services(plain.expose_secret(), |s| (s == old).then(|| new.clone()));
        let resealed =
            env::seal_variable(&state.keys, &state.db, v.environment_id, v.id, &value).await?;
        let keys: Vec<String> = refs::references(&value).iter().map(Ref::key).collect();
        sqlx::query(
            "UPDATE variables SET value_sealed = $2, refs = $3, updated_at = now() WHERE id = $1",
        )
        .bind(v.id)
        .bind(&resealed)
        .bind(&keys)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("UPDATE services SET name = $2 WHERE id = $1")
        .bind(id)
        .bind(&new)
        .execute(&mut *tx)
        .await
        .map_err(|e| match e.as_database_error() {
            Some(d) if d.is_unique_violation() => {
                AppError::Conflict(format!("a service named {new} already exists"))
            }
            _ => e.into(),
        })?;
    // The new name is nobody's alias any more; the old one is this service's.
    sqlx::query(
        "DELETE FROM service_aliases a USING services s
         WHERE s.id = a.service_id AND s.project_id = $1 AND a.name = $2",
    )
    .bind(project)
    .bind(&new)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO service_aliases (service_id, name) VALUES ($1, $2)
         ON CONFLICT (service_id, name) DO UPDATE SET renamed_at = now()",
    )
    .bind(id)
    .bind(&old)
    .execute(&mut *tx)
    .await?;

    // Pending changes: each rewritten variable for its service (a shared one
    // for every service referencing it), and the service itself (its
    // hostname and MODULA_SERVICE_NAME change on its next deploy).
    let environments: Vec<Id> =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE service_id = $1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    let mut changes: BTreeMap<(Id, Option<Id>), Vec<Change>> = BTreeMap::new();
    for env in &environments {
        changes
            .entry((*env, Some(id)))
            .or_default()
            .push(Change::new("service", "updated", "name"));
    }
    for v in &referencing {
        changes
            .entry((v.environment_id, v.service_id))
            .or_default()
            .push(Change::new("variable", "updated", v.name.as_str()));
    }
    for ((env, service), list) in &changes {
        activity::record(&mut tx, *env, *service, Some(actor.user_id), list).await?;
    }
    let mut nodes = Vec::new();
    for env in &environments {
        nodes.extend(mesh::bump_environment(&mut tx, *env).await?);
    }
    Entry::new("service.rename", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service", id)
        .metadata(json!({
            "from": old,
            "to": new,
            "rewrittenVariables": referencing.len(),
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(RenamedService {
        id,
        name: new,
        rewritten_variables: referencing.len(),
    }))
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

/// Settings changes are applied immediately and recorded as a change set;
/// they take effect on the next deploy.
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
    let mut tx = state.db.begin().await?;
    let (env, service, old, template): (Id, Id, serde_json::Value, Option<String>) =
        sqlx::query_as(
            "SELECT i.environment_id, i.service_id, i.config, s.template
             FROM service_instances i JOIN services s ON s.id = i.service_id
             WHERE i.id = $1 FOR UPDATE OF i",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    // A detected start command stays "detected" (a Dockerfile image ignores
    // it) only while it is unchanged; editing it makes it the user's.
    let mut new = new.clone();
    new.start_command_detected = old.get("startCommandDetected") == Some(&json!(true))
        && old.get("startCommand") == serde_json::to_value(&new.start_command).ok().as_ref();
    let new = &new;
    let config = serde_json::to_value(new).map_err(anyhow::Error::from)?;
    // An R2 bucket has nothing to configure, and nothing else can become one.
    let r2 = template.as_deref() == Some(crate::api::buckets::R2_TEMPLATE);
    if r2 == new.deployable() {
        return Err(AppError::bad_request(
            "an R2 bucket's settings can't be changed, and a service can't become one",
        ));
    }
    if new.serverless.is_some() {
        let has_volume: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM volumes WHERE service_instance_id = $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if has_volume {
            return Err(AppError::bad_request(
                "a service with a volume can't be serverless: its data must stay with one running container",
            ));
        }
    }
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoveInstance {
    pub node_id: Id,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Moved {
    pub node_id: Id,
    /// The deployment that runs it on the new server (None: never deployed,
    /// it starts there on its first deploy).
    pub deployment_id: Option<Id>,
}

/// Run the instance on another server: pinned to
/// it, and redeployed there when it was deployed before. The old deployment
/// stops once the new one is live. A volume's data can't follow yet.
pub async fn move_instance(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<MoveInstance>,
) -> AppResult<Json<Moved>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    let node: Option<(String, Option<Id>)> = sqlx::query_as(
        "SELECT name, workspace_id FROM nodes WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(req.node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((node_name, _)) = node.filter(|(_, ws)| ws.is_none_or(|w| w == workspace)) else {
        return Err(AppError::bad_request("no such server"));
    };
    let (current, has_volume, deployed): (Option<Id>, bool, bool) = sqlx::query_as(
        "SELECT i.node_id,
                EXISTS (SELECT 1 FROM volumes v WHERE v.service_instance_id = i.id),
                EXISTS (SELECT 1 FROM deployments d WHERE d.service_instance_id = i.id
                        AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed'))
         FROM service_instances i WHERE i.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if current == Some(req.node_id) {
        return Ok(Json(Moved {
            node_id: req.node_id,
            deployment_id: None,
        }));
    }
    if has_volume {
        return Err(AppError::Conflict(
            "this service has a volume, and moving its data to another server isn't supported yet"
                .into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE service_instances SET node_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(req.node_id)
        .execute(&mut *tx)
        .await?;
    Entry::new("service_instance.move", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", id)
        .metadata(json!({ "from": current, "to": req.node_id }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let deployment_id = if deployed {
        let message = format!("Moved to {node_name}");
        Some(
            crate::api::deployments::deploy_instance(
                &state,
                &actor,
                workspace,
                &meta,
                id,
                Some(&message),
            )
            .await?,
        )
    } else {
        None
    };
    Ok(Json(Moved {
        node_id: req.node_id,
        deployment_id,
    }))
}
