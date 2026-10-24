//! Builds the full desired state for one node from the database. Snapshots are
//! complete (not deltas): the node reconciles to the newest generation it has.

use modula_id::Id;
use modula_proto::node::v1 as pb;
use secrecy::ExposeSecret;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::config::{Builder, RestartPolicy, ServiceConfig};
use super::env;
use crate::config::EdgeTls;
use crate::error::AppResult;
use crate::state::AppState;

const DEFAULT_PIDS: i64 = 4096;
/// Limits for a service that sets none: one runaway container must not take
/// the whole server (and its neighbours) down with it. Raise them per service.
pub const DEFAULT_MEMORY_MB: u32 = 512;
pub const DEFAULT_CPU_MILLIS: u32 = 1000;

#[derive(sqlx::FromRow)]
struct Row {
    id: Id,
    instance_id: Id,
    status: String,
    image: String,
    commit_sha: Option<String>,
    created_at: time::OffsetDateTime,
    upload_sha256: Option<String>,
    workspace_id: Id,
    config: serde_json::Value,
    env_sealed: Option<Vec<u8>>,
    environment_id: Id,
    environment_name: String,
    service_id: Id,
    service_name: String,
    project_id: Id,
    public_domain: Option<String>,
    restart_generation: i32,
    volume_id: Option<Id>,
    mount_path: Option<String>,
    node_public_ip: Option<String>,
    tcp_public_port: Option<i32>,
    mesh_ip: Option<String>,
    /// A read replica's primary service (its name in the environment).
    primary_name: Option<String>,
    /// This replica hasn't come up yet (it still builds, if it must).
    replica_starting: bool,
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    hostname: String,
    deployment_id: Id,
    config: serde_json::Value,
    target_port: Option<i32>,
    remote_ips: Vec<String>,
    proxied: bool,
}

/// SQL: one [`Row`] per deployment replica (`r`), up to the WHERE clause.
macro_rules! workload_rows {
    () => {
        "SELECT d.id, i.id AS instance_id, d.status, d.image, d.commit_sha, d.created_at,
                u.sha256 AS upload_sha256,
                p.workspace_id,
                d.config, d.env_sealed, i.environment_id, e.name AS environment_name,
                s.id AS service_id, s.name AS service_name, s.project_id,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id AND status = 'verified'
                 ORDER BY created_at LIMIT 1) AS public_domain,
                d.restart_generation, v.id AS volume_id, v.mount_path,
                n.public_ip AS node_public_ip, t.public_port AS tcp_public_port,
                r.mesh_ip, r.status = 'starting' AS replica_starting,
                (SELECT ps.name FROM services ps WHERE ps.id = s.replica_of) AS primary_name
         FROM deployment_replicas r
         JOIN deployments d ON d.id = r.deployment_id
         JOIN service_instances i ON i.id = d.service_instance_id
         LEFT JOIN nodes n ON n.id = d.node_id
         LEFT JOIN source_uploads u ON u.id = d.upload_id AND u.purged_at IS NULL
         LEFT JOIN volumes v ON v.service_instance_id = i.id
         LEFT JOIN tcp_proxies t ON t.service_instance_id = i.id
         JOIN environments e ON e.id = i.environment_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id"
    };
}

pub async fn build(state: &AppState, node: Id) -> AppResult<pb::DesiredState> {
    let generation: i64 = sqlx::query_scalar("SELECT desired_generation FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<Row> = sqlx::query_as(concat!(
        workload_rows!(),
        " WHERE r.node_id = $1 AND d.status = ANY($2)
           -- Output hosted elsewhere: the node only builds it.
           AND (d.config->'hosting' IS NULL OR d.status = 'building')
           -- A cron service only until it is ready: its runs are sent one by one.
           AND (d.config->'cronSchedule' IS NULL OR d.status IN ('building', 'deploying'))
         ORDER BY d.created_at"
    ))
    .bind(node)
    .bind(super::state::LIVE)
    .fetch_all(&state.db)
    .await?;

    let mut networks: Vec<Id> = Vec::new();
    let mut workloads = Vec::with_capacity(rows.len());
    for r in rows {
        if !networks.contains(&r.environment_id) {
            networks.push(r.environment_id);
        }
        workloads.push(workload(state, node, r).await?);
    }
    // Cron services run here between snapshots: their networks stay.
    let cron_networks: Vec<Id> = sqlx::query_scalar(
        "SELECT DISTINCT i.environment_id FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         WHERE d.node_id = $1 AND d.status = 'active' AND d.config->'cronSchedule' IS NOT NULL",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    for e in cron_networks {
        if !networks.contains(&e) {
            networks.push(e);
        }
    }
    let pb_routes = routes(state, node).await?;
    // TCP proxies point at the serving deployment, like routes.
    let proxies: Vec<(i32, Id, i32)> = sqlx::query_as(
        "SELECT t.public_port, d.id, t.target_port
         FROM tcp_proxies t
         JOIN deployments d ON d.service_instance_id = t.service_instance_id
         WHERE t.node_id = $1 AND d.node_id = $1 AND d.status IN ('active', 'crashed')
           AND d.config->'hosting' IS NULL AND d.config->'cronSchedule' IS NULL
         ORDER BY t.public_port",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    let volume_removals: Vec<Id> = sqlx::query_scalar(
        "SELECT volume_id FROM volume_removals WHERE node_id = $1 ORDER BY volume_id",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
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
        tcp_proxies: proxies
            .into_iter()
            .map(|(public_port, deployment, target_port)| pb::TcpProxy {
                public_port: public_port as u32,
                deployment_id: deployment.to_string(),
                target_port: target_port as u32,
            })
            .collect(),
        volume_removals: volume_removals.into_iter().map(volume_name).collect(),
        edge: Some(pb::EdgeSettings {
            tls: tls as i32,
            acme_email: state.cfg.acme_email.clone().unwrap_or_default(),
            dashboard_hosts: state.hosts().dashboard.into_iter().collect(),
        }),
        mesh: crate::mesh::build(&state.db, node).await?,
    })
}

/// The workload of `deployment`'s replica on `node`, for a cron run (see
/// `crate::cron`); None when it has none there.
pub async fn cron_workload(
    state: &AppState,
    deployment: Id,
    node: Id,
) -> AppResult<Option<pb::Workload>> {
    let row: Option<Row> = sqlx::query_as(concat!(
        workload_rows!(),
        " WHERE d.id = $1 AND r.node_id = $2"
    ))
    .bind(deployment)
    .bind(node)
    .fetch_optional(&state.db)
    .await?;
    match row {
        Some(r) => Ok(Some(workload(state, node, r).await?)),
        None => Ok(None),
    }
}

/// One deployment replica on `node`, as the node runs it.
async fn workload(state: &AppState, node: Id, r: Row) -> AppResult<pb::Workload> {
    let cfg: ServiceConfig = serde_json::from_value(r.config).map_err(anyhow::Error::from)?;
    let vars = match &r.env_sealed {
        Some(sealed) => {
            env::open_snapshot(&state.keys, &state.db, r.environment_id, r.id, sealed).await?
        }
        None => env::EnvMap::new(),
    };
    let platform = env::Platform {
        port: u64::from(cfg.port),
        project: r.project_id,
        environment: r.environment_id,
        environment_name: &r.environment_name,
        service: r.service_id,
        service_name: &r.service_name,
        deployment: Some(r.id),
        public_domain: r.public_domain.as_deref(),
        tcp_proxy_host: state
            .cfg
            .tcp_proxy_host
            .as_deref()
            .or(r.node_public_ip.as_deref()),
        tcp_proxy_port: r.tcp_public_port,
    };
    // User variables win over platform defaults (e.g. a custom PORT).
    let mut env_vars: Vec<pb::EnvVar> = platform
        .vars()
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
    // Point-in-time recovery: WAL-G's settings win over variables. One that
    // can't be resolved (its storage is gone) leaves the database without
    // it rather than the node without a desired state.
    let pitr = match crate::backup::pitr::workload(state, r.instance_id, r.environment_id, node)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(instance = %r.instance_id, error = ?e, "point-in-time recovery left out");
            None
        }
    };
    // A read replica fills an empty data directory from its primary.
    if let Some(primary) = &r.primary_name {
        env_vars.push(pb::EnvVar {
            name: "MODULA_REPLICA_OF".into(),
            value: format!("{primary}.modula.internal"),
        });
    }
    let pitr = pitr.map(|(vars, mount)| {
        env_vars.retain(|e| !vars.iter().any(|(k, _)| *k == e.name));
        env_vars.extend(
            vars.into_iter()
                .map(|(name, value)| pb::EnvVar { name, value }),
        );
        mount
    });
    // Built images run their baked start command (Railpack wraps it in a
    // shell); everything else takes the configured argv.
    let railpack = cfg.builds_from_source() && cfg.builder == Builder::Railpack;
    let command = match (&cfg.start_command, railpack) {
        (Some(argv), false) => argv.clone(),
        _ => Vec::new(),
    };
    // Where the source comes from: GitHub at the commit, or this server
    // for an upload (a signed download bound to this deployment). Every
    // replica builds on its own server; one that hasn't come up still
    // does after the first replica's build moved the deployment on.
    let builds = r.status == "building"
        || (r.status == "deploying"
            && r.replica_starting
            && r.image.starts_with(super::start::BUILD_PREFIX));
    let source = match (builds, cfg.repo(), cfg.upload(), &r.commit_sha) {
        (true, Some((repo, _, _)), _, Some(sha)) => Some((
            state.github.tarball_url(repo, sha),
            crate::github::app::source_auth(state, r.workspace_id, repo).await?,
            String::new(),
        )),
        (true, None, Some(upload), _) => Some((
            upload_url(state),
            format!(
                "Bearer {}",
                super::upload::token(&state.keys, r.id, upload, r.created_at)
            ),
            // A purged upload has no hash: the download then fails cleanly.
            r.upload_sha256.clone().unwrap_or_else(|| "0".repeat(64)),
        )),
        _ => None,
    };
    // A static site hosted elsewhere: the node uploads the output here.
    let (output_dir, output_url, output_auth) = match cfg.cloudflare() {
        Some(cf) => (
            cf.output_dir.to_owned(),
            output_url(state),
            format!(
                "Bearer {}",
                super::output::token(&state.keys, r.id, r.created_at)
            ),
        ),
        None => Default::default(),
    };
    let build = match source {
        Some((source_url, source_auth, source_sha256)) => Some(pb::Build {
            output_dir,
            output_url,
            output_auth,
            source_url,
            source_auth,
            source_sha256,
            root_dir: cfg
                .root_dir()
                .unwrap_or_default()
                .trim_matches('/')
                .to_owned(),
            builder: match cfg.builder {
                Builder::Railpack => pb::Builder::Railpack,
                Builder::Dockerfile => pb::Builder::Dockerfile,
            } as i32,
            dockerfile_path: cfg.dockerfile_path.clone().unwrap_or_default(),
            build_command: cfg.build_command.clone().unwrap_or_default(),
            start_command: match (&cfg.start_command, railpack) {
                (Some(argv), true) => shell_join(argv),
                _ => String::new(),
            },
            cache_key: r.workspace_id.to_string(),
            secrets_hash: secrets_hash(state, r.environment_id, &vars).await?,
        }),
        None => None,
    };
    Ok(pb::Workload {
        deployment_id: r.id.to_string(),
        service_id: r.service_id.to_string(),
        environment_id: r.environment_id.to_string(),
        service_name: r.service_name,
        image: r.image,
        command,
        env: env_vars,
        port: u32::from(cfg.port),
        health: Some(pb::HealthCheck {
            path: cfg.healthcheck_path.clone().unwrap_or_default(),
            timeout_secs: cfg.healthcheck_timeout_secs,
        }),
        limits: Some(pb::Limits {
            memory_bytes: u64::from(cfg.memory_mb.unwrap_or(DEFAULT_MEMORY_MB)) * 1024 * 1024,
            nano_cpus: u64::from(cfg.cpu_millis.unwrap_or(DEFAULT_CPU_MILLIS)) * 1_000_000,
            pids: DEFAULT_PIDS,
        }),
        restart: match cfg.restart_policy {
            RestartPolicy::OnFailure => pb::RestartPolicy::OnFailure,
            RestartPolicy::Always => pb::RestartPolicy::Always,
            RestartPolicy::Never => pb::RestartPolicy::Never,
        } as i32,
        build,
        restart_generation: u32::try_from(r.restart_generation).unwrap_or(0),
        volume: r
            .volume_id
            .zip(r.mount_path)
            .map(|(id, mount_path)| pb::Volume {
                name: volume_name(id),
                mount_path,
            }),
        pre_deploy_command: cfg.pre_deploy_command.clone().unwrap_or_default(),
        draining_seconds: draining_seconds(&cfg, &vars),
        restart_max_retries: match cfg.restart_policy {
            RestartPolicy::OnFailure => cfg.restart_policy_max_retries.unwrap_or(0),
            _ => 0,
        },
        mesh_ip: r.mesh_ip.unwrap_or_default(),
        cron: cfg.cron_schedule.is_some(),
        pitr,
    })
}

/// The public hostnames this node routes, to the serving deployment only.
async fn routes(state: &AppState, node: Id) -> AppResult<Vec<pb::Route>> {
    // Public hostnames route to the serving deployment only (never one still
    // rolling out), so traffic moves in the same snapshot that retires the old one.
    // Unverified custom domains are left out, which also keeps them off the
    // node's `ask` allowlist (no certificate). Every node running a replica
    // serves them, balancing over its own and the other healthy replicas.
    let routes: Vec<RouteRow> = sqlx::query_as(
        "SELECT m.hostname, d.id AS deployment_id, d.config, m.target_port, m.proxied,
                ARRAY(SELECT o.mesh_ip FROM deployment_replicas o
                      WHERE o.deployment_id = d.id AND o.node_id <> $1 AND o.mesh_ip IS NOT NULL
                        AND o.status = 'active'
                      ORDER BY o.ordinal) AS remote_ips
         FROM domains m
         JOIN deployments d ON d.service_instance_id = m.service_instance_id
         JOIN deployment_replicas r ON r.deployment_id = d.id AND r.node_id = $1
         WHERE d.status IN ('active', 'crashed') AND m.status = 'verified'
           AND d.config->'hosting' IS NULL AND d.config->'cronSchedule' IS NULL
         ORDER BY m.hostname",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
    let mut pb_routes = Vec::with_capacity(routes.len());
    for r in routes {
        let cfg: ServiceConfig = serde_json::from_value(r.config).map_err(anyhow::Error::from)?;
        let port = r
            .target_port
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(u32::from(cfg.port));
        pb_routes.push(pb::Route {
            hostname: r.hostname,
            deployment_id: r.deployment_id.to_string(),
            port,
            remote_upstreams: r
                .remote_ips
                .iter()
                .map(|ip| format!("{ip}:{port}"))
                .collect(),
            // The rules the deployment was made with (staged like the rest).
            rules: super::edge::to_pb(&cfg.edge_rules),
            proxied: r.proxied,
        });
    }
    Ok(pb_routes)
}

/// The configured draining time, or Railway's variable for it when the
/// service sets one (so a migrated app keeps its setting). Capped like the setting.
fn draining_seconds(cfg: &ServiceConfig, vars: &env::EnvMap) -> u32 {
    vars.get("RAILWAY_DEPLOYMENT_DRAINING_SECONDS")
        .and_then(|v| v.expose_secret().trim().parse::<u32>().ok())
        .unwrap_or(cfg.draining_seconds)
        .min(super::config::MAX_DRAINING_SECS)
}

/// Changes whenever a variable value changes, without revealing any: an HMAC
/// under the environment's own key. BuildKit uses it to invalidate layers
/// that read secrets.
async fn secrets_hash(state: &AppState, environment: Id, vars: &env::EnvMap) -> AppResult<String> {
    let key = state
        .keys
        .data_key(&state.db, environment)
        .await?
        .derive("build-secrets-hash");
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).expect("any key length");
    for (k, v) in vars {
        mac.update(k.as_bytes());
        mac.update(b"\0");
        mac.update(v.expose_secret().as_bytes());
        mac.update(b"\0");
    }
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// The URL nodes download uploaded sources from.
fn upload_url(state: &AppState) -> String {
    let base = state.cfg.internal_base().as_str().trim_end_matches('/');
    format!("{base}{}", super::upload::DOWNLOAD_PATH)
}

/// The URL nodes upload build output to.
fn output_url(state: &AppState) -> String {
    let base = state.cfg.internal_base().as_str().trim_end_matches('/');
    format!("{base}{}", super::output::UPLOAD_PATH)
}

/// argv → one POSIX shell string, quoting where needed. Only used for a start
/// command that runs inside the user's own container.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Docker volume backing a `volumes` row.
pub fn volume_name(id: Id) -> String {
    format!("modula-vol-{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes() {
        let argv: Vec<String> = ["node", "server.js", "--name", "a b", "it's", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            shell_join(&argv),
            "node server.js --name 'a b' 'it'\\''s' ''"
        );
    }

    #[test]
    fn railway_draining_variable_overrides_the_setting() {
        let mut cfg: ServiceConfig = serde_json::from_value(serde_json::json!({
            "source": { "type": "image", "image": "api:1" }, "drainingSeconds": 15
        }))
        .unwrap();
        let vars = |pairs: &[(&str, &str)]| -> env::EnvMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned().into()))
                .collect()
        };
        assert_eq!(draining_seconds(&cfg, &vars(&[])), 15);
        let railway = vars(&[("RAILWAY_DEPLOYMENT_DRAINING_SECONDS", "30")]);
        assert_eq!(draining_seconds(&cfg, &railway), 30);
        // Garbage is ignored; values are capped.
        let bad = vars(&[("RAILWAY_DEPLOYMENT_DRAINING_SECONDS", "soon")]);
        assert_eq!(draining_seconds(&cfg, &bad), 15);
        cfg.draining_seconds = 0;
        let big = vars(&[("RAILWAY_DEPLOYMENT_DRAINING_SECONDS", "9999")]);
        assert_eq!(draining_seconds(&cfg, &big), 300);
    }
}
