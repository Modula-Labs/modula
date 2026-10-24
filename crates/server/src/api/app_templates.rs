//! The app template gallery (see `app_templates.rs`): list it, and create
//! every service of an app in one transaction. The client deploys the
//! created instances, as it does for a database template.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::activity::{self, Change};
use crate::api::services::validate_service_name;
use crate::api::{domains, variables};
use crate::app_templates::{self, AppSummary};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

pub async fn list() -> Json<Vec<AppSummary>> {
    Json(
        app_templates::GALLERY
            .iter()
            .map(app_templates::App::summary)
            .collect(),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateApp {
    /// An app id from the gallery.
    pub template: String,
    /// Where the web service gets its generated domain (and the client deploys).
    pub environment_id: Id,
    /// Names become `<prefix>-<role>`; default: the roles.
    pub name_prefix: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedApp {
    pub template: &'static str,
    /// In creation (and deploy) order: databases first.
    pub services: Vec<CreatedAppService>,
    /// The web service's generated domain in the environment.
    pub domain: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedAppService {
    pub id: Id,
    pub name: String,
    pub role: &'static str,
    /// Its instance in the requested environment.
    pub instance_id: Id,
    pub web: bool,
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateApp>,
) -> AppResult<Json<CreatedApp>> {
    let project = grant.id();
    let app = app_templates::get(&req.template)
        .ok_or_else(|| AppError::bad_request("unknown app template"))?;
    let prefix = req
        .name_prefix
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Some(p) = prefix {
        validate_service_name(p)?;
    }
    // The grant covers the project only: the environment must be one of its own.
    let (env_name, env_default): (String, bool) = sqlx::query_as(
        "SELECT e.name, e.id = (SELECT f.id FROM environments f WHERE f.project_id = e.project_id
                                ORDER BY f.created_at, f.id LIMIT 1)
         FROM environments e WHERE e.id = $1 AND e.project_id = $2",
    )
    .bind(req.environment_id)
    .bind(project)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let taken: Vec<String> = sqlx::query_scalar("SELECT name FROM services WHERE project_id = $1")
        .bind(project)
        .fetch_all(&state.db)
        .await?;
    let names = app.service_names(prefix, &taken);
    for n in &names {
        validate_service_name(n)?;
    }
    let name_of = |role: &str| {
        app.services
            .iter()
            .position(|s| s.role == role)
            .map(|i| names[i].clone())
    };

    // One instance per environment, each with its own secrets, sealed with
    // the environment's key before the transaction.
    let envs: Vec<Id> =
        sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1 ORDER BY created_at")
            .bind(project)
            .fetch_all(&state.db)
            .await?;
    struct Planned {
        id: Id,
        config: serde_json::Value,
        instances: Vec<(Id, Id, Vec<variables::SealedVariable>)>,
    }
    let mut planned = Vec::with_capacity(app.services.len());
    for s in app.services {
        let cfg = s.config();
        cfg.validate_for_instance()?;
        let mut instances = Vec::with_capacity(envs.len());
        for env in &envs {
            let vars = variables::seal_new(&state, *env, &s.variables(name_of)).await?;
            instances.push((*env, Id::new(), vars));
        }
        planned.push(Planned {
            id: Id::new(),
            config: serde_json::to_value(&cfg).map_err(anyhow::Error::from)?,
            instances,
        });
    }

    let apps_domain = state.apps_domain();
    let mut warnings = Vec::new();
    let mut domain = None;
    let mut created = Vec::with_capacity(planned.len());
    let mut tx = state.db.begin().await?;
    for ((s, name), p) in app.services.iter().zip(&names).zip(&planned) {
        let inserted = sqlx::query(
            "INSERT INTO services (id, project_id, name, template, app_template) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (project_id, name) DO NOTHING",
        )
        .bind(p.id)
        .bind(project)
        .bind(name)
        .bind(s.template)
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(AppError::Conflict(format!(
                "a service named {name} already exists"
            )));
        }
        let mut here = None;
        for (env, instance, vars) in &p.instances {
            sqlx::query(
                "INSERT INTO service_instances (id, service_id, environment_id, config) VALUES ($1, $2, $3, $4)",
            )
            .bind(instance)
            .bind(p.id)
            .bind(env)
            .bind(&p.config)
            .execute(&mut *tx)
            .await?;
            variables::insert_new(&mut tx, *env, *instance, vars).await?;
            if let Some(mount) = s.mount_path() {
                sqlx::query(
                    "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, $3)",
                )
                .bind(Id::new())
                .bind(instance)
                .bind(mount)
                .execute(&mut *tx)
                .await?;
            }
            let changes = [Change::new("service", "created", name.as_str())];
            activity::record(&mut tx, *env, Some(p.id), Some(actor.user_id), &changes).await?;
            if *env == req.environment_id {
                here = Some(*instance);
            }
        }
        let instance_id = here.expect("every environment gets an instance");
        if s.domain {
            match &apps_domain {
                Some(apps) => {
                    let (_, host) = domains::insert_generated(
                        &mut tx,
                        instance_id,
                        name,
                        (!env_default).then_some(env_name.as_str()),
                        apps,
                    )
                    .await?;
                    domain = Some(host);
                }
                None => warnings.push(format!(
                    "{name} has no domain yet: add an apps domain in Settings → Domain, then generate one"
                )),
            }
        }
        Entry::new("service.create", &meta)
            .actor(&actor)
            .workspace(grant.workspace_id()?)
            .target("service", p.id)
            .metadata(json!({
                "project": project,
                "name": name,
                "source": s.image(),
                "template": s.template,
                "appTemplate": app.id,
            }))
            .record(&mut *tx)
            .await?;
        created.push(CreatedAppService {
            id: p.id,
            name: name.clone(),
            role: s.role,
            instance_id,
            web: s.domain,
        });
    }
    tx.commit().await?;
    Ok(Json(CreatedApp {
        template: app.id,
        services: created,
        domain,
        warnings,
    }))
}
