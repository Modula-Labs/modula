//! The deployment state machine, driven by events from the node that runs it.
//!
//!   building ──BUILT──▶ deploying ──ACTIVE──▶ active ──CRASHED──▶ crashed ──ACTIVE──▶ active
//!      │                   │                   │
//!    FAILED              FAILED            superseded by a newer active deployment
//!      ▼                   ▼                   ▼
//!    failed              failed             removed
//!
//! Image deployments start at `deploying`; repository deployments at `building`.
//! A static site or function hosted elsewhere (`config.hosting`) leaves its
//! node at BUILT: the node has uploaded the output (see `deploy::output`),
//! and the server publishes it while the deployment is `deploying`, then
//! makes it active or failed itself (see `integrations::cloudflare::publish`).
//! A function hosted elsewhere with its code inline never reaches a node:
//! it is published from `deploying` right away.
//!
//! A node may only report on deployments placed on it; anything else is ignored.
//!
//! Replicas: a deployment with several replicas runs one on each of several
//! nodes (`deployment_replicas`). Each node reports on its own; the build of
//! replica 0 (the deployment's node) is the one recorded. The deployment is
//! active once every replica is, failed when any fails, and crashed when no
//! replica is running any more. An active deployment with a replica that
//! isn't running (its node reported it crashed) reads as degraded
//! ([`degraded_sql!`](crate::degraded_sql)): still `active` in the table,
//! serving from the others; the crash fires `deployment.degraded` webhooks
//! and `deployment_degraded` alert rules.
//! Replica changes re-push the environment's
//! address book to every node running something of it.
//!
//! Repository deployments report `pending` / `success` / `failure` on their
//! commit through the job queue (never inline: GitHub must not slow this path),
//! and a pull request preview's on its comment.
//!
//! A push to a service with `waitForCi` starts at `waiting_ci`, placed but
//! not on any node's desired state, until the commit's other checks pass
//! (`building`) or fail or time out (`failed`); see `github::ci`.
//!
//! Serverless deployments (`config.serverless`, one replica) scale to zero:
//!
//!   active ──SLEEPING──▶ sleeping ──ACTIVE (woken, cold start ms)──▶ active
//!
//! `sleeping` is live: still the serving deployment (routed, in the desired
//! state, retired like `active`), with its container stopped until a request
//! wakes it. It is never a crash, and no webhook or commit status is sent.

use modula_id::Id;
use modula_proto::node::v1::{DeploymentEvent, DeploymentPhase};
use sqlx::PgPool;

use super::build_output;
use super::config::ServiceConfig;
use crate::error::AppResult;
use crate::jobs::{self, NewJob};

pub const LIVE: &[&str] = &["building", "deploying", "active", "sleeping", "crashed"];

/// SQL (a boolean) for "deployment `d` is active but a replica of it isn't
/// running" (its node reported it crashed, or it hasn't come back yet).
#[macro_export]
macro_rules! degraded_sql {
    () => {
        "(d.status = 'active' AND EXISTS (
             SELECT 1 FROM deployment_replicas dr
             WHERE dr.deployment_id = d.id AND dr.status <> 'active'))"
    };
}

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
        DeploymentPhase::Sleeping => "sleeping",
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
    // Lock the row and check ownership: a node can only touch deployments
    // with a replica placed on it.
    let row: Option<(Id, String, bool, serde_json::Value, i16, String, Id)> = sqlx::query_as(
        "SELECT d.service_instance_id, d.status,
                d.commit_sha IS NOT NULL AND d.trigger IN ('push', 'manual')
                  AND EXISTS (SELECT 1 FROM github_apps),
                d.config, r.ordinal, r.status, i.environment_id
         FROM deployments d
         JOIN deployment_replicas r ON r.deployment_id = d.id AND r.node_id = $2
         JOIN service_instances i ON i.id = d.service_instance_id
         WHERE d.id = $1 FOR UPDATE OF d",
    )
    .bind(dep)
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((instance, status, reports_commit, config, ordinal, replica_status, environment)) =
        row
    else {
        tracing::warn!(%node, deployment = %dep, "event for a deployment not placed on this node; ignored");
        return Ok(vec![]);
    };
    let primary = ordinal == 0;
    let replicas: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM deployment_replicas WHERE deployment_id = $1 AND node_id IS NOT NULL",
    )
    .bind(dep)
    .fetch_one(&mut *tx)
    .await?;
    let shown = if replicas > 1 {
        format!("Replica {}: {message}", ordinal + 1)
    } else {
        message.clone()
    };
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, $2, $3)",
    )
    .bind(dep)
    .bind(phase_name(phase))
    .bind(&shown)
    .execute(&mut *tx)
    .await?;

    let mut kick = Vec::new();
    let mut commit_status = None;
    // This replica's own state, for discovery and the rollout below.
    let replica_now = match phase {
        DeploymentPhase::Active => Some("active"),
        DeploymentPhase::Crashed => Some("crashed"),
        DeploymentPhase::Failed => Some("failed"),
        _ => None,
    };
    if let Some(now) = replica_now.filter(|now| *now != replica_status) {
        sqlx::query(
            "UPDATE deployment_replicas SET status = $3, updated_at = now()
             WHERE deployment_id = $1 AND ordinal = $2",
        )
        .bind(dep)
        .bind(ordinal)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        kick.extend(crate::mesh::bump_environment(&mut tx, environment).await?);
    }
    let running: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM deployment_replicas
         WHERE deployment_id = $1 AND node_id IS NOT NULL AND status = 'active'",
    )
    .bind(dep)
    .fetch_one(&mut *tx)
    .await?;
    match (phase, status.as_str()) {
        // Only replica 0's build is recorded; the others build the same commit.
        (DeploymentPhase::Building | DeploymentPhase::Built, _) if !primary => {}
        // Not live until every replica is.
        (DeploymentPhase::Active, "deploying") if running < replicas => {}
        // Others still run: the deployment keeps serving, degraded.
        (DeploymentPhase::Crashed, "active") if running > 0 && replica_status == "active" => {
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentDegraded)
                .await?;
            let event = crate::alerts::Event {
                kind: crate::alerts::Kind::DeploymentDegraded,
                service_instance_id: instance,
                deployment_id: Some(*dep),
                backup_id: None,
            };
            crate::alerts::enqueue_event(&mut *tx, event).await?;
        }
        (DeploymentPhase::Crashed, "active") if running > 0 => {}
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
        (DeploymentPhase::Built, "building") if config.get("hosting").is_some() => {
            let sha256 = message.trim().to_ascii_lowercase();
            sqlx::query(
                "UPDATE deployments SET status = 'deploying', build_finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .execute(&mut *tx)
            .await?;
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
            crate::integrations::cloudflare::publish::enqueue(&mut tx, *dep, &sha256).await?;
            // The build is off the node's desired state now.
            kick.push(node);
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
        (DeploymentPhase::Active, "deploying" | "crashed") if config.get("hosting").is_none() => {
            let old = activate(&mut tx, instance, *dep).await?;
            // Runs on other servers than before: custom domains' A records
            // follow (one per server running a replica).
            let mut before = old.clone();
            before.sort();
            before.dedup();
            let mut now = replica_nodes(&mut tx, *dep).await?;
            now.sort();
            now.dedup();
            if !before.is_empty() && before != now {
                let job = NewJob {
                    kind: crate::domains::RESYNC_JOB,
                    payload: serde_json::json!({ "serviceInstanceId": instance }),
                    ..Default::default()
                };
                jobs::enqueue(&mut *tx, job).await?;
            }
            kick.extend(old);
            commit_status = Some("success");
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentSucceeded)
                .await?;
            // Routes and the address book now point at the new deployment.
            kick.push(node);
            kick.extend(crate::mesh::bump_environment(&mut tx, environment).await?);
        }
        (DeploymentPhase::Failed, was @ ("building" | "deploying")) => {
            sqlx::query(
                "UPDATE deployments SET status = 'failed', error = $2, finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(&shown)
            .execute(&mut *tx)
            .await?;
            // Every replica stops (it left the desired state).
            kick.extend(replica_nodes(&mut tx, *dep).await?);
            kick.extend(crate::mesh::bump_environment(&mut tx, environment).await?);
            commit_status = Some("failure");
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentFailed)
                .await?;
            let event = crate::alerts::Event {
                kind: crate::alerts::failure_kind(was),
                service_instance_id: instance,
                deployment_id: Some(*dep),
                backup_id: None,
            };
            crate::alerts::enqueue_event(&mut *tx, event).await?;
        }
        (DeploymentPhase::Sleeping, "active") => {
            sqlx::query(
                "UPDATE deployments SET status = 'sleeping', updated_at = now() WHERE id = $1",
            )
            .bind(dep)
            .execute(&mut *tx)
            .await?;
            // It leaves (and below, rejoins) the private network's names.
            kick.extend(crate::mesh::bump_environment(&mut tx, environment).await?);
        }
        // Woken by a request (or found running again).
        (DeploymentPhase::Active, "sleeping" | "active")
            if status == "sleeping" || ev.cold_start_ms > 0 =>
        {
            let cold = i32::try_from(ev.cold_start_ms).ok().filter(|ms| *ms > 0);
            sqlx::query(
                "UPDATE deployments SET status = 'active', updated_at = now(),
                        woke_at = CASE WHEN $2::integer IS NULL THEN woke_at ELSE now() END,
                        cold_start_ms = coalesce($2, cold_start_ms)
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(cold)
            .execute(&mut *tx)
            .await?;
            if status == "sleeping" {
                kick.extend(crate::mesh::bump_environment(&mut tx, environment).await?);
            }
        }
        (DeploymentPhase::Crashed, "active" | "sleeping") => {
            // Stays in the desired state so the restart policy can recover it.
            sqlx::query("UPDATE deployments SET status = 'crashed', error = $2, updated_at = now() WHERE id = $1")
                .bind(dep)
                .bind(&shown)
                .execute(&mut *tx)
                .await?;
            crate::webhooks::enqueue(&mut tx, *dep, crate::webhooks::Event::DeploymentCrashed)
                .await?;
            let event = crate::alerts::Event {
                kind: crate::alerts::Kind::DeploymentCrashed,
                service_instance_id: instance,
                deployment_id: Some(*dep),
                backup_id: None,
            };
            crate::alerts::enqueue_event(&mut *tx, event).await?;
        }
        _ => {}
    }
    if let Some(s) = commit_status {
        crate::github::report(&mut tx, *dep, s, reports_commit).await?;
    }
    kick.sort();
    kick.dedup();
    for n in &kick {
        super::bump(&mut *tx, *n).await?;
    }
    tx.commit().await?;
    Ok(kick)
}

/// The nodes running a replica of `deployment`.
async fn replica_nodes(tx: &mut sqlx::PgConnection, deployment: Id) -> AppResult<Vec<Id>> {
    Ok(sqlx::query_scalar(
        "SELECT node_id FROM deployment_replicas WHERE deployment_id = $1 AND node_id IS NOT NULL",
    )
    .bind(deployment)
    .fetch_all(&mut *tx)
    .await?)
}

/// Makes `dep` its instance's serving deployment, retiring the previous one
/// in the same transaction (the one-active-per-instance unique index requires
/// it). Returns the nodes the retired deployments' replicas ran on.
pub async fn activate(tx: &mut sqlx::PgConnection, instance: Id, dep: Id) -> AppResult<Vec<Id>> {
    let old: Vec<Option<Id>> = sqlx::query_scalar(
        "WITH old AS (
             UPDATE deployments SET status = 'removed', finished_at = now(), updated_at = now()
             WHERE service_instance_id = $1 AND status IN ('active', 'sleeping', 'crashed') AND id <> $2
             RETURNING id)
         SELECT r.node_id FROM deployment_replicas r JOIN old ON old.id = r.deployment_id",
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
    Ok(old.into_iter().flatten().collect())
}

/// Stack ids are short slugs (`nextjs`, `python`); anything else from a node is dropped.
fn valid_stack(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
