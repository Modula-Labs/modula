//! One environment as config as code (`modula config pull/plan`):
//! every service's settings, domains, volume, TCP proxy and variables in one
//! read. Values of non-sealed variables are included (a Member can read them
//! this way without sudo, like database credentials), so every read is
//! audited with the names it returned. Sealed values never leave the server.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::json;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::env;
use crate::error::AppResult;
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::templates;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfig {
    pub id: Id,
    pub name: String,
    pub project_id: Id,
    pub services: Vec<ServiceEntry>,
    /// The environment's shared variables (`${{shared.NAME}}`).
    pub shared: Vec<VariableEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEntry {
    pub service_id: Id,
    pub service_name: String,
    pub instance_id: Id,
    pub template: Option<String>,
    /// What a template service starts from (its settings' defaults).
    pub template_config: Option<ServiceConfig>,
    /// Variable names the template creates (managed by the template).
    pub template_variables: Vec<&'static str>,
    pub config: serde_json::Value,
    pub volume: Option<serde_json::Value>,
    pub tcp_proxy: Option<serde_json::Value>,
    pub domains: Vec<DomainEntry>,
    pub variables: Vec<VariableEntry>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DomainEntry {
    pub id: Id,
    #[serde(skip)]
    pub service_instance_id: Id,
    pub hostname: String,
    /// `generated` or `custom`.
    pub kind: String,
    pub status: String,
    pub target_port: Option<i32>,
    pub rate_limit_per_minute: Option<i32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariableEntry {
    pub id: Id,
    pub name: String,
    pub sealed: bool,
    /// None for sealed variables.
    pub value: Option<String>,
}

#[derive(sqlx::FromRow)]
struct InstanceRow {
    id: Id,
    service_id: Id,
    service_name: String,
    template: Option<String>,
    config: serde_json::Value,
    volume: Option<serde_json::Value>,
    tcp_proxy: Option<serde_json::Value>,
}

#[derive(sqlx::FromRow)]
struct VariableRow {
    id: Id,
    service_instance_id: Option<Id>,
    name: String,
    sealed: bool,
    value_sealed: Vec<u8>,
}

pub async fn get(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<EnvironmentConfig>> {
    let environment = grant.id();
    let (name, project_id): (String, Id) =
        sqlx::query_as("SELECT name, project_id FROM environments WHERE id = $1")
            .bind(environment)
            .fetch_one(&state.db)
            .await?;
    let instances: Vec<InstanceRow> = sqlx::query_as(
        "SELECT i.id, i.service_id, s.name AS service_name, s.template, i.config,
                (SELECT json_build_object('id', v.id, 'mountPath', v.mount_path)
                 FROM volumes v WHERE v.service_instance_id = i.id) AS volume,
                (SELECT json_build_object('id', t.id, 'publicPort', t.public_port, 'targetPort', t.target_port)
                 FROM tcp_proxies t WHERE t.service_instance_id = i.id) AS tcp_proxy
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         WHERE i.environment_id = $1
         ORDER BY s.created_at",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    let domains: Vec<DomainEntry> = sqlx::query_as(
        "SELECT d.id, d.service_instance_id, d.hostname, d.kind, d.status, d.target_port, d.rate_limit_per_minute
         FROM domains d JOIN service_instances i ON i.id = d.service_instance_id
         WHERE i.environment_id = $1 ORDER BY d.created_at",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;
    let rows: Vec<VariableRow> = sqlx::query_as(
        "SELECT id, service_instance_id, name, sealed, value_sealed FROM variables
         WHERE environment_id = $1 ORDER BY name",
    )
    .bind(environment)
    .fetch_all(&state.db)
    .await?;

    let mut revealed = Vec::new();
    let mut variables = Vec::with_capacity(rows.len());
    for r in rows {
        let value = if r.sealed {
            None
        } else {
            let plain =
                env::open_variable(&state.keys, &state.db, environment, r.id, &r.value_sealed)
                    .await?;
            revealed.push(r.name.clone());
            Some(plain.expose_secret().to_owned())
        };
        variables.push((
            r.service_instance_id,
            VariableEntry {
                id: r.id,
                name: r.name,
                sealed: r.sealed,
                value,
            },
        ));
    }

    let mut shared = Vec::new();
    let mut by_instance: std::collections::HashMap<Id, Vec<VariableEntry>> =
        std::collections::HashMap::new();
    for (instance, v) in variables {
        match instance {
            Some(i) => by_instance.entry(i).or_default().push(v),
            None => shared.push(v),
        }
    }
    let mut domains_by_instance: std::collections::HashMap<Id, Vec<DomainEntry>> =
        std::collections::HashMap::new();
    for d in domains {
        domains_by_instance
            .entry(d.service_instance_id)
            .or_default()
            .push(d);
    }
    let services = instances
        .into_iter()
        .map(|i| {
            let template = i.template.as_deref().and_then(templates::get);
            ServiceEntry {
                service_id: i.service_id,
                service_name: i.service_name,
                instance_id: i.id,
                template_config: template.map(templates::Template::config),
                template_variables: template
                    .map(|t| t.variables.iter().map(|(k, _)| *k).collect())
                    .unwrap_or_default(),
                template: i.template,
                config: i.config,
                volume: i.volume,
                tcp_proxy: i.tcp_proxy,
                domains: domains_by_instance.remove(&i.id).unwrap_or_default(),
                variables: by_instance.remove(&i.id).unwrap_or_default(),
            }
        })
        .collect();

    Entry::new("environment.config_read", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("environment", environment)
        .metadata(json!({ "revealed": revealed }))
        .record(&state.db)
        .await?;
    Ok(Json(EnvironmentConfig {
        id: environment,
        name,
        project_id,
        services,
        shared,
    }))
}
