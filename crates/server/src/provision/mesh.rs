//! Keeping provider firewalls in step with the private network: WireGuard
//! is open only to Modula's own servers, so whenever a server joins or
//! leaves it [`SYNC_JOB`] asks every connected provider to update its rule.

use modula_id::Id;
use serde_json::json;

use super::{MeshSources, provider_for, provider_kinds};
use crate::error::AppResult;
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

pub const SYNC_JOB: &str = "provision.sync_mesh";
/// The job's former kind: ones queued before an upgrade still run.
pub const LEGACY_SYNC_JOB: &str = "hetzner.sync_firewalls";

/// Public IPs of the servers in the private network that peer with
/// `workspace`'s (its own and shared ones), or with anyone (None: the main
/// server's firewall), including provisioned servers still joining.
pub async fn mesh_sources(db: &sqlx::PgPool, workspace: Option<Id>) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT public_ip FROM nodes
         WHERE status <> 'revoked' AND wg_public_key IS NOT NULL AND public_ip IS NOT NULL
           AND ($1::text IS NULL OR workspace_id IS NULL OR workspace_id = $1)
         UNION
         SELECT ipv4 FROM provisioned_servers
         WHERE ipv4 IS NOT NULL AND status NOT IN ('failed', 'deleting')
           AND ($1::text IS NULL OR workspace_id = $1)
         ORDER BY 1",
    )
    .bind(workspace)
    .fetch_all(db)
    .await?)
}

/// Public IPs of this instance's main servers (the nodes running the
/// dashboard): the only main firewalls a sync may touch.
pub async fn main_addresses(db: &sqlx::PgPool) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT public_ip FROM nodes
         WHERE status <> 'revoked' AND public_ip IS NOT NULL
           AND coalesce(info->'capabilities' ? 'dashboard', false)
         ORDER BY 1",
    )
    .fetch_all(db)
    .await?)
}

/// Queues [`SYNC_JOB`] (once: queued syncs coalesce).
pub async fn enqueue_sync(db: impl sqlx::PgExecutor<'_>) -> AppResult<()> {
    jobs::enqueue(
        db,
        NewJob {
            kind: SYNC_JOB,
            payload: json!({}),
            max_attempts: Some(3),
            dedupe_key: Some(SYNC_JOB),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Job: every connected integration of every provider updates its firewalls.
/// One failing doesn't stop the others; the job then fails, to be retried.
pub async fn sync_job(state: AppState, _job: Job) -> anyhow::Result<()> {
    sync_integrations(&state, None).await
}

/// Update the firewalls of `workspaces`' connected providers now, or of every
/// workspace's with None: what a migration needs done before its first copy.
pub async fn sync_integrations(state: &AppState, workspaces: Option<&[Id]>) -> anyhow::Result<()> {
    let only: Option<Vec<String>> = workspaces.map(|w| w.iter().map(ToString::to_string).collect());
    let connected: Vec<(Id, String)> = sqlx::query_as(
        "SELECT workspace_id, kind FROM integrations
         WHERE kind = ANY($1) AND ($2::text[] IS NULL OR workspace_id::text = ANY($2))",
    )
    .bind(provider_kinds())
    .bind(only)
    .fetch_all(&state.db)
    .await?;
    let everyone = mesh_sources(&state.db, None).await?;
    let mains = main_addresses(&state.db).await?;
    let mut failed = Vec::new();
    for (workspace, kind) in connected {
        let provider = match provider_for(state, workspace, &kind).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                failed.push(format!("{kind} in {workspace}: {e}"));
                continue;
            }
        };
        let sources = MeshSources {
            workspace: mesh_sources(&state.db, Some(workspace)).await?,
            everyone: everyone.clone(),
            mains: mains.clone(),
        };
        if let Err(e) = provider.sync_mesh(&sources).await {
            failed.push(format!("{kind} in {workspace}: {e}"));
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("cannot update provider firewalls: {}", failed.join("; "));
    }
    Ok(())
}
