//! Rolling upgrades of the other servers to the control plane's version.
//!
//! Once this server runs a newer version than a node, the node is asked
//! (`UpgradeRequest` on its stream) to install this version; the next one is
//! asked only after it came back on the new version. Nodes that don't offer
//! the `upgrade` capability (older releases) are left alone: an old node keeps
//! working with a newer server, and the dashboard says to upgrade it by hand.
//! A failure stops the rollout until an admin retries it.

use std::time::Duration;

use modula_id::Id;
use modula_proto::node::v1 as pb;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::{VERSION, release};
use crate::state::AppState;

/// How long a node may take to download, switch and come back.
pub const NODE_TIMEOUT: time::Duration = time::Duration::minutes(15);
pub const TICK: Duration = Duration::from_secs(10);

pub async fn run(state: AppState, stop: CancellationToken, tick: Duration) {
    loop {
        if let Err(e) = step(&state).await {
            tracing::warn!(error = format!("{e:#}"), "server upgrades");
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(tick) => {}
        }
    }
}

/// One pass: finish the upgrade in flight, or start the next one.
pub async fn step(state: &AppState) -> anyhow::Result<()> {
    let running: Option<(Id, Id, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, node_id, to_version, started_at FROM node_upgrades WHERE status = 'running'
         ORDER BY started_at LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some((id, node, to, started)) = running {
        let version: Option<String> = sqlx::query_scalar(
            "SELECT node_version FROM nodes WHERE id = $1 AND status = 'online'",
        )
        .bind(node)
        .fetch_optional(&state.db)
        .await?
        .flatten();
        if version.as_deref() == Some(to.as_str()) && state.hub.is_connected(node) {
            finish(state, id, None).await?;
            tracing::info!(%node, version = %to, "server upgraded");
        } else if started + NODE_TIMEOUT < OffsetDateTime::now_utc() {
            finish(
                state,
                id,
                Some("did not come back on the new version within 15 minutes"),
            )
            .await?;
        }
        return Ok(());
    }

    // A failure pauses the rollout (the rest would likely fail the same way).
    let paused: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM node_upgrades u, instance_updates s
                        WHERE u.status = 'failed' AND u.to_version = $1
                          AND (s.rollout_cleared_at IS NULL OR u.started_at > s.rollout_cleared_at))",
    )
    .bind(VERSION)
    .fetch_one(&state.db)
    .await?;
    if paused {
        return Ok(());
    }
    let candidates: Vec<(Id, String, Option<String>)> = sqlx::query_as(
        "SELECT id, name, node_version FROM nodes
         WHERE status = 'online' AND info->'capabilities' ? 'upgrade'
         ORDER BY name, id",
    )
    .fetch_all(&state.db)
    .await?;
    let next = candidates.into_iter().find(|(id, _, v)| {
        v.as_deref().is_some_and(|v| release::is_newer(VERSION, v)) && state.hub.is_connected(*id)
    });
    let Some((node, name, from)) = next else {
        return Ok(());
    };
    let id = Id::new();
    sqlx::query(
        "INSERT INTO node_upgrades (id, node_id, from_version, to_version) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(node)
    .bind(&from)
    .bind(VERSION)
    .execute(&state.db)
    .await?;
    let req = pb::server_message::Msg::Upgrade(pb::UpgradeRequest {
        request_id: id.to_string(),
        version: VERSION.to_owned(),
    });
    if state.hub.send(node, req).await {
        tracing::info!(%node, %name, from = ?from, to = VERSION, "upgrading server");
    } else {
        finish(
            state,
            id,
            Some("the server disconnected before it was asked"),
        )
        .await?;
    }
    Ok(())
}

async fn finish(state: &AppState, id: Id, error: Option<&str>) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE node_upgrades SET status = $2, error = $3, finished_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(if error.is_some() {
        "failed"
    } else {
        "succeeded"
    })
    .bind(error)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// A node's answer. Success needs no action (its new Hello completes it);
/// a failure ends its upgrade and pauses the rollout.
pub async fn on_status(state: &AppState, node: Id, s: pb::UpgradeStatus) -> anyhow::Result<()> {
    if s.ok {
        return Ok(());
    }
    let Ok(id) = s.request_id.parse::<Id>() else {
        return Ok(());
    };
    let error: String = s.error.chars().take(2000).collect();
    tracing::warn!(%node, error = %error, "server upgrade failed");
    sqlx::query(
        "UPDATE node_upgrades SET status = 'failed', error = $3, finished_at = now()
         WHERE id = $1 AND node_id = $2 AND status = 'running'",
    )
    .bind(id)
    .bind(node)
    .bind(error)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Resume a paused rollout (failed servers are asked again).
pub async fn retry(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("UPDATE instance_updates SET rollout_cleared_at = now()")
        .execute(&state.db)
        .await?;
    Ok(())
}
