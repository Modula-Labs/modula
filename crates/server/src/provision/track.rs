//! Following a provisioned server until its node joins: a job that polls the
//! provider and the node registry, re-enqueueing itself every few seconds
//! until the server is `ready` or `failed`.

use std::time::Duration;

use modula_id::Id;
use serde_json::json;

use crate::error::AppResult;
use crate::jobs::{self, Job, NewJob};
use crate::provision::{Phase, ServerProvider};
use crate::state::AppState;

pub const TRACK_JOB: &str = "provision.track";
const POLL: Duration = Duration::from_secs(5);
/// A row still `creating` after this lost its create call (e.g. a restart).
const CREATE_GRACE: time::Duration = time::Duration::minutes(2);

pub async fn enqueue(state: &AppState, id: Id, after: Duration) -> AppResult<()> {
    let run_at = time::OffsetDateTime::now_utc() + after;
    jobs::enqueue(
        &state.db,
        NewJob {
            kind: TRACK_JOB,
            payload: json!({ "id": id }),
            run_at: Some(run_at),
            max_attempts: Some(3),
            dedupe_key: None,
        },
    )
    .await?;
    state.jobs_notify.notify_one();
    Ok(())
}

pub async fn track_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let id: Id = serde_json::from_value(job.payload["id"].clone())?;
    let done = poll_once(&state, id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if !done {
        enqueue(&state, id, POLL)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Row {
    workspace_id: Id,
    status: String,
    provider_id: Option<String>,
    provider_action_id: Option<String>,
    join_token_id: Option<Id>,
    created_at: time::OffsetDateTime,
}

async fn set_status(state: &AppState, id: Id, status: &str, error: Option<&str>) -> AppResult<()> {
    sqlx::query(
        "UPDATE provisioned_servers SET status = $2, error = $3, updated_at = now()
         WHERE id = $1 AND status NOT IN ('ready', 'failed', 'deleting')",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// One step. Returns true once there is nothing left to follow.
pub async fn poll_once(state: &AppState, id: Id) -> AppResult<bool> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT workspace_id, status, provider_id, provider_action_id, join_token_id, created_at
         FROM provisioned_servers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else { return Ok(true) };
    if matches!(row.status.as_str(), "ready" | "failed" | "deleting") {
        return Ok(true);
    }
    let Some(provider) = crate::integrations::hetzner::provider(state, row.workspace_id).await?
    else {
        set_status(state, id, "failed", Some("Hetzner was disconnected")).await?;
        return Ok(true);
    };

    // The create call never recorded its result: look for the server by label.
    let Some(server_id) = row.provider_id.clone() else {
        if time::OffsetDateTime::now_utc() - row.created_at < CREATE_GRACE {
            return Ok(false);
        }
        match provider.find(&id.to_string()).await? {
            Some(s) => {
                sqlx::query(
                    "UPDATE provisioned_servers SET provider_id = $2, ipv4 = $3, status = 'booting',
                         updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(&s.id)
                .bind(&s.ipv4)
                .execute(&state.db)
                .await?;
                return Ok(false);
            }
            None => {
                set_status(state, id, "failed", Some("the server was not created")).await?;
                return Ok(true);
            }
        }
    };

    if row.status == "creating" || row.status == "booting" {
        let s = provider
            .status(&server_id, row.provider_action_id.as_deref())
            .await?;
        if let Some(err) = s.error {
            set_status(state, id, "failed", Some(&err)).await?;
            return Ok(true);
        }
        match s.phase {
            Phase::Gone => {
                set_status(
                    state,
                    id,
                    "failed",
                    Some("the server was deleted at Hetzner"),
                )
                .await?;
                return Ok(true);
            }
            Phase::Running => {
                sqlx::query(
                    "UPDATE provisioned_servers SET status = 'joining', ipv4 = COALESCE($2, ipv4),
                         updated_at = now() WHERE id = $1 AND status IN ('creating', 'booting')",
                )
                .bind(id)
                .bind(&s.ipv4)
                .execute(&state.db)
                .await?;
            }
            Phase::Booting | Phase::Off => return Ok(false),
        }
    }

    // Joining: enrollment records the node on the join token.
    let token: Option<(Option<Id>, bool)> =
        sqlx::query_as("SELECT node_id, expires_at < now() FROM node_join_tokens WHERE id = $1")
            .bind(row.join_token_id)
            .fetch_optional(&state.db)
            .await?;
    match token {
        Some((Some(node), _)) => {
            let online: bool = state.hub.is_connected(node)
                || sqlx::query_scalar::<_, bool>(
                    "SELECT status = 'online' FROM nodes WHERE id = $1",
                )
                .bind(node)
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(false);
            sqlx::query(
                "UPDATE provisioned_servers SET node_id = $2,
                     status = CASE WHEN $3 THEN 'ready' ELSE status END, updated_at = now()
                 WHERE id = $1 AND status = 'joining'",
            )
            .bind(id)
            .bind(node)
            .bind(online)
            .execute(&state.db)
            .await?;
            Ok(online)
        }
        Some((None, true)) | None => {
            set_status(
                state,
                id,
                "failed",
                Some("the server did not join within 30 minutes; see /var/log/cloud-init-output.log on it"),
            )
            .await?;
            Ok(true)
        }
        Some((None, false)) => Ok(false),
    }
}
