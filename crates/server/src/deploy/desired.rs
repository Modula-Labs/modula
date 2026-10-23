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

#[derive(sqlx::FromRow)]
struct Row {
    id: Id,
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
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    hostname: String,
    deployment_id: Id,
    config: serde_json::Value,
    target_port: Option<i32>,
}

pub async fn build(state: &AppState, node: Id) -> AppResult<pb::DesiredState> {
    let generation: i64 = sqlx::query_scalar("SELECT desired_generation FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT d.id, d.status, d.image, d.commit_sha, d.created_at, u.sha256 AS upload_sha256,
                p.workspace_id,
                d.config, d.env_sealed, i.environment_id, e.name AS environment_name,
                s.id AS service_id, s.name AS service_name, s.project_id,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id AND status = 'verified'
                 ORDER BY created_at LIMIT 1) AS public_domain,
                d.restart_generation, v.id AS volume_id, v.mount_path,
                n.public_ip AS node_public_ip, t.public_port AS tcp_public_port
         FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN nodes n ON n.id = d.node_id
         LEFT JOIN source_uploads u ON u.id = d.upload_id AND u.purged_at IS NULL
         LEFT JOIN volumes v ON v.service_instance_id = i.id
         LEFT JOIN tcp_proxies t ON t.service_instance_id = i.id
         JOIN environments e ON e.id = i.environment_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE d.node_id = $1 AND d.status = ANY($2)
         ORDER BY d.created_at",
    )
    .bind(node)
    .bind(super::state::LIVE)
    .fetch_all(&state.db)
    .await?;

    let mut networks: Vec<Id> = Vec::new();
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
            (
                "MODULA_TCP_PROXY_DOMAIN",
                tcp_proxy_host(state, r.node_public_ip.as_deref(), r.tcp_public_port),
            ),
            (
                "MODULA_TCP_PROXY_PORT",
                r.tcp_public_port.map(|p| p.to_string()).unwrap_or_default(),
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
        // Built images run their baked start command (Railpack wraps it in a
        // shell); everything else takes the configured argv.
        let railpack = cfg.builds_from_source() && cfg.builder == Builder::Railpack;
        let command = match (&cfg.start_command, railpack) {
            (Some(argv), false) => argv.clone(),
            _ => Vec::new(),
        };
        // Where the source comes from: GitHub at the commit, or this server
        // for an upload (a signed download bound to this deployment).
        let source = match (r.status.as_str(), cfg.repo(), cfg.upload(), &r.commit_sha) {
            ("building", Some((repo, _, _)), _, Some(sha)) => Some((
                state.github.tarball_url(repo, sha),
                crate::github::app::source_auth(state, r.workspace_id, repo).await?,
                String::new(),
            )),
            ("building", None, Some(upload), _) => Some((
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
        let build = match source {
            Some((source_url, source_auth, source_sha256)) => Some(pb::Build {
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
        if !networks.contains(&r.environment_id) {
            networks.push(r.environment_id);
        }
        workloads.push(pb::Workload {
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
                memory_bytes: cfg.memory_mb.map_or(0, |m| u64::from(m) * 1024 * 1024),
                nano_cpus: cfg.cpu_millis.map_or(0, |c| u64::from(c) * 1_000_000),
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
        });
    }
    // Public hostnames route to the serving deployment only (never one still
    // rolling out), so traffic moves in the same snapshot that retires the old one.
    // Unverified custom domains are left out, which also keeps them off the
    // node's `ask` allowlist (no certificate).
    let routes: Vec<RouteRow> = sqlx::query_as(
        "SELECT m.hostname, d.id AS deployment_id, d.config, m.target_port
         FROM domains m
         JOIN deployments d ON d.service_instance_id = m.service_instance_id
         WHERE d.node_id = $1 AND d.status IN ('active', 'crashed') AND m.status = 'verified'
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
            port: r
                .target_port
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or(u32::from(cfg.port)),
        });
    }
    // TCP proxies point at the serving deployment, like routes.
    let proxies: Vec<(i32, Id, i32)> = sqlx::query_as(
        "SELECT t.public_port, d.id, t.target_port
         FROM tcp_proxies t
         JOIN deployments d ON d.service_instance_id = t.service_instance_id
         WHERE t.node_id = $1 AND d.node_id = $1 AND d.status IN ('active', 'crashed')
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
        }),
    })
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

/// Where clients reach a TCP proxy: the configured host, else the node's IP.
pub fn tcp_proxy_host(state: &AppState, node_public_ip: Option<&str>, port: Option<i32>) -> String {
    match port {
        None => String::new(),
        Some(_) => state
            .cfg
            .tcp_proxy_host
            .clone()
            .or_else(|| node_public_ip.map(str::to_owned))
            .unwrap_or_default(),
    }
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
