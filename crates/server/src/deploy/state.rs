//! The deployment state machine, driven by events from the node that runs it.
//!
//!   building ──BUILT──▶ deploying ──ACTIVE──▶ active ──CRASHED──▶ crashed ──ACTIVE──▶ active
//!      │                   │                   │
//!    FAILED              FAILED            superseded by a newer active deployment
//!      ▼                   ▼                   ▼
//!    failed              failed             removed
//!
//! Image deployments start at `deploying`; repository deployments at `building`.
//!
//! A node may only report on deployments placed on it; anything else is ignored.
//!
//! Repository deployments report `pending` / `success` / `failure` on their
//! commit through the job queue (never inline: GitHub must not slow this path).

use modula_id::Id;
use modula_proto::node::v1::{DeploymentEvent, DeploymentPhase};
use sqlx::PgPool;

use super::build_output;
use super::config::ServiceConfig;
use crate::error::AppResult;
use crate::jobs::{self, NewJob};

pub const LIVE: &[&str] = &["building", "deploying", "active", "crashed"];

pub fn phase_name(p: DeploymentPhase) -> &'static str {
    match p {
        DeploymentPhase::PhaseUnspecified => "unspecified",
        DeploymentPhase::Building => "building",
        DeploymentPhase::Built => "built",
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
pub async fn apply_event(db: &PgPool, node: Id, ev: &DeploymentEvent) -> AppResult<Vec<Id>> {
    let Ok(dep) = &ev.deployment_id.parse::<Id>() else {
        return Ok(vec![]);
    };
    let phase = DeploymentPhase::try_from(ev.phase).unwrap_or(DeploymentPhase::PhaseUnspecified);
    let message: String = ev.message.chars().take(2000).collect();
    let mut tx = db.begin().await?;
    // Lock the row and check ownership: a node can only touch its own deployments.
    let row: Option<(Id, String, bool, serde_json::Value)> = sqlx::query_as(
        "SELECT service_instance_id, status,
                commit_sha IS NOT NULL AND trigger IN ('push', 'manual')
                  AND EXISTS (SELECT 1 FROM github_app),
                config
         FROM deployments WHERE id = $1 AND node_id = $2 FOR UPDATE",
    )
    .bind(dep)
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((instance, status, reports_commit, config)) = row else {
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
    let mut commit_status = None;
    match (phase, status.as_str()) {
        (DeploymentPhase::Building, "building") => {
            commit_status = Some("pending");
            sqlx::query(
                "UPDATE deployments SET build_started_at = coalesce(build_started_at, now()), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .execute(&mut *tx)
            .await?;
        }
        (DeploymentPhase::Built, "building") => {
            // Only a well-formed image id is recorded; rollbacks run exactly it.
            let digest = message.strip_prefix("sha256:").filter(|h| {
                h.len() == 64
                    && h.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            });
            // The repository's config file may have changed how it runs; the
            // snapshot records that so a restart or redeploy matches.
            let config = match (
                &ev.build,
                serde_json::from_value::<ServiceConfig>(config.clone()),
            ) {
                (Some(out), Ok(cfg)) => serde_json::to_value(build_output::apply(cfg, out))
                    .map_err(anyhow::Error::from)?,
                _ => config,
            };
            sqlx::query(
                "UPDATE deployments SET status = 'deploying', image_digest = $2, config = $3,
                        build_finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(digest.map(|h| format!("sha256:{h}")))
            .bind(&config)
            .execute(&mut *tx)
            .await?;
            kick.extend(super::start::retire_for_volume(&mut tx, instance, Some(*dep)).await?);
            if let Some(stack) = ev
                .build
                .as_ref()
                .map(|b| b.stack.as_str())
                .filter(|s| valid_stack(s))
            {
                sqlx::query("UPDATE service_instances SET stack = $2 WHERE id = $1")
                    .bind(instance)
                    .bind(stack)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        (DeploymentPhase::Active, "deploying" | "crashed") => {
            // Supersede the previous active deployment in the same transaction:
            // the one-active-per-instance unique index requires it.
            let old: Vec<Option<Id>> = sqlx::query_scalar(
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
            // Moved to other servers: custom domains' A records follow.
            if old.iter().flatten().any(|n| *n != node) {
                let job = NewJob {
                    kind: crate::domains::RESYNC_JOB,
                    payload: serde_json::json!({ "serviceInstanceId": instance }),
                    ..Default::default()
                };
                jobs::enqueue(&mut *tx, job).await?;
            }
            kick.extend(old.into_iter().flatten());
            commit_status = Some("success");
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentSucceeded)
                .await?;
            // This node's routes now point at the new deployment.
            kick.push(node);
        }
        (DeploymentPhase::Failed, "building" | "deploying") => {
            sqlx::query(
                "UPDATE deployments SET status = 'failed', error = $2, finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(&message)
            .execute(&mut *tx)
            .await?;
            kick.push(node);
            commit_status = Some("failure");
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentFailed)
                .await?;
        }
        (DeploymentPhase::Crashed, "active") => {
            // Stays in the desired state so the restart policy can recover it.
            sqlx::query("UPDATE deployments SET status = 'crashed', error = $2, updated_at = now() WHERE id = $1")
                .bind(dep)
                .bind(&message)
                .execute(&mut *tx)
                .await?;
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentCrashed)
                .await?;
        }
        _ => {}
    }
    if let (true, Some(s)) = (reports_commit, commit_status) {
        let job = NewJob {
            kind: crate::github::app::STATUS_JOB,
            payload: serde_json::json!({ "deploymentId": dep, "state": s }),
            ..Default::default()
        };
        jobs::enqueue(&mut *tx, job).await?;
    }
    kick.sort();
    kick.dedup();
    for n in &kick {
        super::bump(&mut *tx, *n).await?;
    }
    tx.commit().await?;
    Ok(kick)
}

/// Stack ids are short slugs (`nextjs`, `python`); anything else from a node is dropped.
fn valid_stack(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
