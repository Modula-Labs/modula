//! The deployment state machine, driven by events from the node that runs it.
//!
//!   deploying ──ACTIVE──▶ active ──CRASHED──▶ crashed ──ACTIVE──▶ active
//!       │                   │
//!     FAILED            superseded by a newer active deployment
//!       ▼                   ▼
//!     failed             removed
//!
//! A node may only report on deployments placed on it; anything else is ignored.

use modula_proto::node::v1::{DeploymentEvent, DeploymentPhase};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

pub const LIVE: &[&str] = &["deploying", "active", "crashed"];

pub fn phase_name(p: DeploymentPhase) -> &'static str {
    match p {
        DeploymentPhase::PhaseUnspecified => "unspecified",
        DeploymentPhase::Pulling => "pulling",
        DeploymentPhase::Creating => "creating",
        DeploymentPhase::Starting => "starting",
        DeploymentPhase::Healthchecking => "healthchecking",
        DeploymentPhase::Active => "active",
        DeploymentPhase::Failed => "failed",
        DeploymentPhase::Crashed => "crashed",
        DeploymentPhase::Stopped => "stopped",
    }
}

/// Apply one event. Returns nodes whose desired state changed (to kick).
pub async fn apply_event(db: &PgPool, node: Uuid, ev: &DeploymentEvent) -> AppResult<Vec<Uuid>> {
    let Ok(dep) = Uuid::parse_str(&ev.deployment_id) else {
        return Ok(vec![]);
    };
    let phase = DeploymentPhase::try_from(ev.phase).unwrap_or(DeploymentPhase::PhaseUnspecified);
    let message: String = ev.message.chars().take(2000).collect();
    let mut tx = db.begin().await?;
    // Lock the row and check ownership: a node can only touch its own deployments.
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT service_instance_id, status FROM deployments WHERE id = $1 AND node_id = $2 FOR UPDATE",
    )
    .bind(dep)
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((instance, status)) = row else {
        tracing::warn!(%node, deployment = %dep, "event for a deployment not placed on this node; ignored");
        return Ok(vec![]);
    };
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, $2, $3)",
    )
    .bind(dep)
    .bind(phase_name(phase))
    .bind(&message)
    .execute(&mut *tx)
    .await?;

    let mut kick = Vec::new();
    match (phase, status.as_str()) {
        (DeploymentPhase::Active, "deploying" | "crashed") => {
            // Supersede the previous active deployment in the same transaction:
            // the one-active-per-instance unique index requires it.
            let old: Vec<Option<Uuid>> = sqlx::query_scalar(
                "UPDATE deployments SET status = 'removed', finished_at = now(), updated_at = now()
                 WHERE service_instance_id = $1 AND status IN ('active', 'crashed') AND id <> $2
                 RETURNING node_id",
            )
            .bind(instance)
            .bind(dep)
            .fetch_all(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE deployments SET status = 'active', error = NULL, updated_at = now() WHERE id = $1",
            )
            .bind(dep)
            .execute(&mut *tx)
            .await?;
            kick.extend(old.into_iter().flatten());
            // This node's routes now point at the new deployment.
            kick.push(node);
        }
        (DeploymentPhase::Failed, "deploying") => {
            sqlx::query(
                "UPDATE deployments SET status = 'failed', error = $2, finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(&message)
            .execute(&mut *tx)
            .await?;
            kick.push(node);
        }
        (DeploymentPhase::Crashed, "active") => {
            // Stays in the desired state so the restart policy can recover it.
            sqlx::query("UPDATE deployments SET status = 'crashed', error = $2, updated_at = now() WHERE id = $1")
                .bind(dep)
                .bind(&message)
                .execute(&mut *tx)
                .await?;
        }
        _ => {}
    }
    kick.sort();
    kick.dedup();
    for n in &kick {
        super::bump(&mut *tx, *n).await?;
    }
    tx.commit().await?;
    Ok(kick)
}
