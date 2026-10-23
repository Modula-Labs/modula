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
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Push => "push",
            Self::Redeploy => "redeploy",
            Self::Rollback => "rollback",
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
    /// Run this exact, already-built image instead of building.
    pub prebuilt: Option<Prebuilt>,
    /// The deployment a redeploy or rollback repeats.
    pub source_deployment: Option<Id>,
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
    let vars = env::resolve(&state.keys, &state.db, env_id, d.instance).await?;
    let env_sealed = env::seal_snapshot(&state.keys, &state.db, env_id, id, &vars).await?;
    let service_id: Id =
        sqlx::query_scalar("SELECT service_id FROM service_instances WHERE id = $1 FOR UPDATE")
            .bind(d.instance)
            .fetch_one(&mut *tx)
            .await?;

    // What runs: an image as configured, a rebuilt-before image, or a build.
    let (status, image, digest) = match (&d.prebuilt, d.config.repo()) {
        (Some(p), _) => ("deploying", p.image.clone(), p.digest.clone()),
        (None, Some(_)) => ("building", build_tag(service_id, id), None),
        (None, None) => ("deploying", d.config.source_label(), None),
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
    let commit = d.commit.as_ref();
    sqlx::query(
        "INSERT INTO deployments (id, service_instance_id, node_id, status, image, image_digest, config,
                                  env_sealed, trigger, created_by, commit_sha, commit_message, commit_author,
                                  commit_author_login, source_deployment_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
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
    .bind(commit.map(|c| &c.sha))
    .bind(commit.map(|c| &c.message))
    .bind(commit.map(|c| &c.author))
    .bind(commit.and_then(|c| c.author_login.as_ref()))
    .bind(d.source_deployment)
    .execute(&mut *tx)
    .await?;
    let what = match commit {
        Some(c) => format!("Building {} at {}", d.config.source_label(), &c.sha[..7]),
        None => format!("Deploying {image}"),
    };
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'queued', $2)",
    )
    .bind(id)
    .bind(what)
    .execute(&mut *tx)
    .await?;
    let mut kick: Vec<Id> = superseded.into_iter().flatten().collect();
    kick.push(node);
    kick.sort();
    kick.dedup();
    for n in &kick {
        bump(&mut *tx, *n).await?;
    }
    Ok(Started { id, node, kick })
}
