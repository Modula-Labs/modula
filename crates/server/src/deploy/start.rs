//! Starting a deployment: snapshot the instance's config and variables, place
//! it on a node, supersede anything still rolling out, and bump the nodes.
//! Shared by manual deploys, pushes (webhooks) and rollbacks.

use modula_id::Id;

use super::config::ServiceConfig;
use super::{bump, env, placement};
use crate::error::AppResult;
use crate::github::Commit;
use crate::state::AppState;

/// Why a deployment exists (shown in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Push,
    Redeploy,
    Rollback,
    /// `modula up`: files uploaded from a developer's machine.
    Upload,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Push => "push",
            Self::Redeploy => "redeploy",
            Self::Rollback => "rollback",
            Self::Upload => "upload",
        }
    }
}

pub struct NewDeployment {
    pub instance: Id,
    pub config: ServiceConfig,
    pub trigger: Trigger,
    pub created_by: Option<Id>,
    /// The commit to build (repository sources).
    pub commit: Option<Commit>,
    /// Uploaded sources: the local Git state the CLI reported.
    pub local_git: Option<LocalGit>,
    /// Run this exact, already-built image instead of building.
    pub prebuilt: Option<Prebuilt>,
    /// The deployment a redeploy or rollback repeats.
    pub source_deployment: Option<Id>,
}

/// Where an uploaded source came from, as the CLI saw it (display only;
/// stored in the commit columns, which never drive a build for uploads).
#[derive(Debug, Clone, Default)]
pub struct LocalGit {
    /// Full lowercase hex sha of the local HEAD.
    pub commit: Option<String>,
    /// `up -m`, else the local commit's subject.
    pub message: Option<String>,
    pub branch: Option<String>,
    /// Uncommitted changes were included.
    pub dirty: Option<bool>,
}

pub struct Prebuilt {
    pub image: String,
    pub digest: Option<String>,
}

pub struct Started {
    pub id: Id,
    pub node: Id,
    /// Nodes whose desired state changed; kick them after the commit.
    pub kick: Vec<Id>,
}

/// Node-local tag for an image built from source. Never pulled.
pub fn build_tag(service_id: Id, deployment_id: Id) -> String {
    format!("{BUILD_PREFIX}{service_id}:{deployment_id}")
}
pub const BUILD_PREFIX: &str = "modula-build/";

pub async fn start(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    env_id: Id,
    d: NewDeployment,
) -> AppResult<Started> {
    d.config.validate()?;
    let id = Id::new();
    let resolved = env::resolve_for_deployment(
        &state.keys,
        &state.db,
        state.cfg.tcp_proxy_host.as_deref(),
        env_id,
        d.instance,
        Some(id),
    )
    .await?;
    let env_sealed = env::seal_snapshot(&state.keys, &state.db, env_id, id, &resolved.vars).await?;
    let raw = env::raw_variables(&state.keys, &state.db, env_id, d.instance).await?;
    let vars_sealed = env::seal_vars_snapshot(&state.keys, &state.db, env_id, id, &raw).await?;
    let service_id: Id =
        sqlx::query_scalar("SELECT service_id FROM service_instances WHERE id = $1 FOR UPDATE")
            .bind(d.instance)
            .fetch_one(&mut *tx)
            .await?;

    // What runs: an image as configured, a rebuilt-before image, or a build.
    let (status, image, digest) = match (&d.prebuilt, d.config.builds_from_source()) {
        (Some(p), _) => ("deploying", p.image.clone(), p.digest.clone()),
        (None, true) => ("building", build_tag(service_id, id), None),
        (None, false) => ("deploying", d.config.source_label(), None),
    };
    let node = placement::choose(&mut *tx, d.instance).await?;
    // A newer deployment supersedes any still in progress (the running one
    // keeps serving until the new one is healthy).
    let superseded: Vec<Option<Id>> = sqlx::query_scalar(
        "UPDATE deployments SET status = 'cancelled', finished_at = now(), updated_at = now()
         WHERE service_instance_id = $1 AND status IN ('queued', 'building', 'deploying') RETURNING node_id",
    )
    .bind(d.instance)
    .fetch_all(&mut *tx)
    .await?;
    // A service with a volume can't run twice on the same data: an image
    // deployment retires the serving one now (a build does so once built,
    // see state.rs), and the node stops it before starting the new one.
    let retired = if status == "deploying" {
        retire_for_volume(&mut *tx, d.instance, None).await?
    } else {
        Vec::new()
    };
    let commit = d.commit.as_ref();
    let local = d.local_git.as_ref();
    sqlx::query(
        "INSERT INTO deployments (id, service_instance_id, node_id, status, image, image_digest, config,
                                  env_sealed, trigger, created_by, commit_sha, commit_message, commit_author,
                                  commit_author_login, source_deployment_id, upload_id, source_branch,
                                  source_dirty, vars_sealed)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
    )
    .bind(id)
    .bind(d.instance)
    .bind(node)
    .bind(status)
    .bind(&image)
    .bind(&digest)
    .bind(serde_json::to_value(&d.config).map_err(anyhow::Error::from)?)
    .bind(&env_sealed)
    .bind(d.trigger.as_str())
    .bind(d.created_by)
    .bind(commit.map(|c| &c.sha).or(local.and_then(|g| g.commit.as_ref())))
    .bind(commit.map(|c| &c.message).or(local.and_then(|g| g.message.as_ref())))
    .bind(commit.map(|c| &c.author))
    .bind(commit.and_then(|c| c.author_login.as_ref()))
    .bind(d.source_deployment)
    .bind(d.config.upload())
    .bind(local.and_then(|g| g.branch.as_ref()))
    .bind(local.and_then(|g| g.dirty))
    .bind(&vars_sealed)
    .execute(&mut *tx)
    .await?;
    let what = match (commit, d.config.upload()) {
        (_, Some(_)) if d.prebuilt.is_none() => "Building uploaded files".to_owned(),
        (Some(c), None) => format!("Building {} at {}", d.config.source_label(), &c.sha[..7]),
        _ => format!("Deploying {image}"),
    };
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'queued', $2)",
    )
    .bind(id)
    .bind(what)
    .execute(&mut *tx)
    .await?;
    if !resolved.missing.is_empty() {
        sqlx::query(
            "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'warning', $2)",
        )
        .bind(id)
        .bind(format!(
            "These referenced variables don't exist and were left empty: {}",
            resolved.missing.join(", ")
        ))
        .execute(&mut *tx)
        .await?;
    }
    let mut kick: Vec<Id> = superseded.into_iter().flatten().chain(retired).collect();
    kick.push(node);
    kick.sort();
    kick.dedup();
    for n in &kick {
        bump(&mut *tx, *n).await?;
    }
    Ok(Started { id, node, kick })
}

/// For a service with a volume, mark its serving deployment removed (other
/// than `keep`). Returns the nodes to kick. No-op without a volume.
pub async fn retire_for_volume(
    tx: &mut sqlx::PgConnection,
    instance: Id,
    keep: Option<Id>,
) -> AppResult<Vec<Id>> {
    let nodes: Vec<Option<Id>> = sqlx::query_scalar(
        "UPDATE deployments SET status = 'removed', finished_at = now(), updated_at = now()
         WHERE service_instance_id = $1 AND status IN ('active', 'crashed')
           AND id IS DISTINCT FROM $2
           AND EXISTS (SELECT 1 FROM volumes WHERE service_instance_id = $1)
         RETURNING node_id",
    )
    .bind(instance)
    .bind(keep)
    .fetch_all(&mut *tx)
    .await?;
    Ok(nodes.into_iter().flatten().collect())
}
