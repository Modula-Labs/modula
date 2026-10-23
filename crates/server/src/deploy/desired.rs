//! Builds the full desired state for one node from the database. Snapshots are
//! complete (not deltas): the node reconciles to the newest generation it has.

use modula_proto::node::v1 as pb;
use secrecy::ExposeSecret;
use uuid::Uuid;

use super::config::{RestartPolicy, ServiceConfig};
use super::env;
use crate::config::EdgeTls;
use crate::error::AppResult;
use crate::state::AppState;

const DEFAULT_PIDS: i64 = 4096;

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    config: serde_json::Value,
    env_sealed: Option<Vec<u8>>,
    environment_id: Uuid,
    environment_name: String,
    service_id: Uuid,
    service_name: String,
    project_id: Uuid,
    public_domain: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    hostname: String,
    deployment_id: Uuid,
    config: serde_json::Value,
}

pub async fn build(state: &AppState, node: Uuid) -> AppResult<pb::DesiredState> {
    let generation: i64 = sqlx::query_scalar("SELECT desired_generation FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT d.id, d.config, d.env_sealed, i.environment_id, e.name AS environment_name,
                s.id AS service_id, s.name AS service_name, s.project_id,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id
                 ORDER BY created_at LIMIT 1) AS public_domain
         FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN environments e ON e.id = i.environment_id
         JOIN services s ON s.id = i.service_id
         WHERE d.node_id = $1 AND d.status = ANY($2)
         ORDER BY d.created_at",
    )
    .bind(node)
    .bind(super::state::LIVE)
    .fetch_all(&state.db)
    .await?;

    let mut networks: Vec<Uuid> = Vec::new();
    let mut workloads = Vec::with_capacity(rows.len());
    for r in rows {
        let cfg: ServiceConfig = serde_json::from_value(r.config).map_err(anyhow::Error::from)?;
        let vars = match &r.env_sealed {
            Some(sealed) => {
                env::open_snapshot(&state.keys, &state.db, r.environment_id, r.id, sealed).await?
            }
            None => env::EnvMap::new(),
        };
        let platform = [
            ("PORT", cfg.port.to_string()),
            ("MODULA_PROJECT_ID", r.project_id.to_string()),
            ("MODULA_ENVIRONMENT_ID", r.environment_id.to_string()),
            ("MODULA_ENVIRONMENT_NAME", r.environment_name.clone()),
            ("MODULA_SERVICE_ID", r.service_id.to_string()),
            ("MODULA_SERVICE_NAME", r.service_name.clone()),
            ("MODULA_DEPLOYMENT_ID", r.id.to_string()),
            (
                "MODULA_PRIVATE_DOMAIN",
                format!("{}.modula.internal", r.service_name),
            ),
            (
                "MODULA_PUBLIC_DOMAIN",
                r.public_domain.clone().unwrap_or_default(),
            ),
        ];
        // User variables win over platform defaults (e.g. a custom PORT);
        // MODULA_* names are reserved, so only PORT can actually collide.
        let mut env_vars: Vec<pb::EnvVar> = platform
            .into_iter()
            .filter(|(k, _)| !vars.contains_key(*k))
            .map(|(k, v)| pb::EnvVar {
                name: k.into(),
                value: v,
            })
            .collect();
        env_vars.extend(vars.iter().map(|(k, v)| pb::EnvVar {
            name: k.clone(),
            value: v.expose_secret().to_owned(),
        }));
        if !networks.contains(&r.environment_id) {
            networks.push(r.environment_id);
        }
        workloads.push(pb::Workload {
            deployment_id: r.id.to_string(),
            service_id: r.service_id.to_string(),
            environment_id: r.environment_id.to_string(),
            service_name: r.service_name,
            image: cfg.image().to_owned(),
            command: cfg.start_command.clone().unwrap_or_default(),
            env: env_vars,
            port: u32::from(cfg.port),
            health: Some(pb::HealthCheck {
                path: cfg.healthcheck_path.clone().unwrap_or_default(),
                timeout_secs: cfg.healthcheck_timeout_secs,
            }),
            limits: Some(pb::Limits {
                memory_bytes: cfg.memory_mb.map_or(0, |m| u64::from(m) * 1024 * 1024),
                nano_cpus: cfg.cpu_millis.map_or(0, |c| u64::from(c) * 1_000_000),
                pids: DEFAULT_PIDS,
            }),
            restart: match cfg.restart_policy {
                RestartPolicy::OnFailure => pb::RestartPolicy::OnFailure,
                RestartPolicy::Always => pb::RestartPolicy::Always,
                RestartPolicy::Never => pb::RestartPolicy::Never,
            } as i32,
        });
    }
    // Public hostnames route to the serving deployment only (never one still
    // rolling out), so traffic moves in the same snapshot that retires the old one.
    let routes: Vec<RouteRow> = sqlx::query_as(
        "SELECT m.hostname, d.id AS deployment_id, d.config
         FROM domains m
         JOIN deployments d ON d.service_instance_id = m.service_instance_id
         WHERE d.node_id = $1 AND d.status IN ('active', 'crashed')
         ORDER BY m.hostname",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    let mut pb_routes = Vec::with_capacity(routes.len());
    for r in routes {
        let cfg: ServiceConfig = serde_json::from_value(r.config).map_err(anyhow::Error::from)?;
        pb_routes.push(pb::Route {
            hostname: r.hostname,
            deployment_id: r.deployment_id.to_string(),
            port: u32::from(cfg.port),
        });
    }
    let tls = match state.cfg.edge_tls {
        EdgeTls::Acme => pb::TlsMode::TlsAcme,
        EdgeTls::Internal => pb::TlsMode::TlsInternal,
        EdgeTls::Off => pb::TlsMode::TlsOff,
    };

    Ok(pb::DesiredState {
        generation: generation as u64,
        networks: networks
            .into_iter()
            .map(|e| pb::Network {
                environment_id: e.to_string(),
            })
            .collect(),
        workloads,
        routes: pb_routes,
        edge: Some(pb::EdgeSettings {
            tls: tls as i32,
            acme_email: state.cfg.acme_email.clone().unwrap_or_default(),
        }),
    })
}
