//! Cron services (`ServiceConfig.cronSchedule`, Railway's "Cron Schedule").
//!
//! A cron service's deployment builds (or pulls) and becomes active like any
//! other, but nothing runs continuously: its node reports ACTIVE without
//! starting a container and the deployment leaves the desired state. Then,
//! at each due time, [`tick`] starts a run: a `cron_runs` row and a
//! `CronRun` to the deployment's server (where a built image exists), which
//! runs the workload once (the node's `Runtime::run_once`), ships its output
//! as the deployment's logs and answers with `CronResult`.
//!
//! `cron_schedules` holds the next due time per instance, kept in step with
//! the serving deployment's schedule (so a schedule change applies when it
//! is deployed). Claiming a due time moves it on with a compare-and-set, so
//! two server replicas never both start one. One run at a time per instance
//! (a unique index): a due time while one runs is recorded as skipped.

pub mod schedule;

use std::time::Duration;

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::Serialize;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::deploy::config::ServiceConfig;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub use schedule::{describe, next_after, parse, upcoming};

pub const TICK: Duration = Duration::from_secs(10);
/// A run with no result this long after its timeout is given up on (the
/// node's own timeout stops it; this covers a lost node or result).
const RESULT_GRACE_SECS: i32 = 300;
/// Runs kept per instance in the list.
const KEEP_DAYS: i32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: Id,
    pub deployment_id: Option<Id>,
    pub trigger: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scheduled_for: Option<OffsetDateTime>,
    pub timeout_secs: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

macro_rules! run_columns {
    () => {
        "SELECT id, deployment_id, trigger, status, scheduled_for, timeout_secs, created_at,
                started_at, finished_at, exit_code, error FROM cron_runs"
    };
}

pub async fn load(state: &AppState, id: Id) -> AppResult<Run> {
    sqlx::query_as(concat!(run_columns!(), " WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// An instance's newest runs, newest first.
pub async fn list(state: &AppState, instance: Id, limit: i64) -> AppResult<Vec<Run>> {
    Ok(sqlx::query_as(concat!(
        run_columns!(),
        " WHERE service_instance_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2"
    ))
    .bind(instance)
    .bind(limit)
    .fetch_all(&state.db)
    .await?)
}

/// The next due time of an instance's deployed schedule, if it has one.
pub async fn next_run_at(state: &AppState, instance: Id) -> AppResult<Option<OffsetDateTime>> {
    Ok(
        sqlx::query_scalar("SELECT next_run_at FROM cron_schedules WHERE service_instance_id = $1")
            .bind(instance)
            .fetch_optional(&state.db)
            .await?,
    )
}

pub async fn schedule_loop(state: AppState, stop: CancellationToken) {
    let mut tick_every = tokio::time::interval(TICK);
    tick_every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick_every.tick() => {
                if let Err(e) = tick(&state).await {
                    tracing::warn!(error = format!("{e:#}"), "cron tick failed");
                }
            }
        }
    }
}

/// One pass: follow deployed schedules, start due runs, give up on runs
/// that never reported back, forget old ones.
pub async fn tick(state: &AppState) -> anyhow::Result<()> {
    sync_schedules(state).await?;
    for (instance, due) in claim_due(state, OffsetDateTime::now_utc()).await? {
        match start(state, instance, Trigger::Schedule, Some(due), None).await {
            Ok(_) | Err(AppError::Conflict(_)) => {}
            Err(e) => tracing::warn!(%instance, error = ?e, "could not start a cron run"),
        }
    }
    sweep(state).await?;
    Ok(())
}

/// `cron_schedules` = one row per instance whose serving deployment has a
/// schedule, with that schedule. A changed schedule starts over from now.
async fn sync_schedules(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM cron_schedules c WHERE NOT EXISTS (
             SELECT 1 FROM deployments d
             WHERE d.service_instance_id = c.service_instance_id AND d.status = 'active'
               AND d.config->>'cronSchedule' = c.schedule)",
    )
    .execute(&state.db)
    .await?;
    let missing: Vec<(Id, String)> = sqlx::query_as(
        "SELECT d.service_instance_id, d.config->>'cronSchedule' FROM deployments d
         WHERE d.status = 'active' AND d.config->>'cronSchedule' IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM cron_schedules c
                           WHERE c.service_instance_id = d.service_instance_id)",
    )
    .fetch_all(&state.db)
    .await?;
    let now = OffsetDateTime::now_utc();
    for (instance, expr) in missing {
        let Some(next) = parse(&expr).ok().and_then(|c| next_after(&c, now)) else {
            tracing::warn!(%instance, schedule = %expr, "a deployed cron schedule has no next time");
            continue;
        };
        sqlx::query(
            "INSERT INTO cron_schedules (service_instance_id, schedule, next_run_at)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(instance)
        .bind(&expr)
        .bind(next)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// Due times reached by `now`, each claimed by moving it to the next one
/// after `now` (a compare-and-set: only one caller wins a due time). Due
/// times missed while the server was down run once, not once per miss.
pub async fn claim_due(
    state: &AppState,
    now: OffsetDateTime,
) -> anyhow::Result<Vec<(Id, OffsetDateTime)>> {
    let due: Vec<(Id, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT service_instance_id, schedule, next_run_at FROM cron_schedules
         WHERE next_run_at <= $1",
    )
    .bind(now)
    .fetch_all(&state.db)
    .await?;
    let mut claimed = Vec::new();
    for (instance, expr, at) in due {
        let Some(next) = parse(&expr).ok().and_then(|c| next_after(&c, now)) else {
            continue;
        };
        let won: Option<Id> = sqlx::query_scalar(
            "UPDATE cron_schedules SET next_run_at = $3
             WHERE service_instance_id = $1 AND next_run_at = $2
             RETURNING service_instance_id",
        )
        .bind(instance)
        .bind(at)
        .bind(next)
        .fetch_optional(&state.db)
        .await?;
        if won.is_some() {
            claimed.push((instance, at));
        }
    }
    Ok(claimed)
}

/// Start one run of `instance` from its serving deployment. A scheduled run
/// while another runs is recorded as skipped; a manual one is a conflict.
pub async fn start(
    state: &AppState,
    instance: Id,
    trigger: Trigger,
    scheduled_for: Option<OffsetDateTime>,
    user: Option<Id>,
) -> AppResult<Id> {
    let serving: Option<(Id, Option<Id>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, node_id, config FROM deployments
         WHERE service_instance_id = $1 AND status = 'active'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let Some((deployment, node, config)) = serving else {
        return Err(AppError::Conflict(
            "this service has no successful deployment to run yet".into(),
        ));
    };
    let cfg: ServiceConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    if cfg.cron_schedule.is_none() {
        return Err(AppError::Conflict(
            "this service's deployment has no cron schedule".into(),
        ));
    }
    let timeout = cfg.cron_timeout();
    let id = Id::new();
    let queued: Option<Id> = sqlx::query_scalar(
        "INSERT INTO cron_runs (id, service_instance_id, deployment_id, node_id, trigger, status,
                                scheduled_for, timeout_secs, created_by)
         VALUES ($1, $2, $3, $4, $5, 'queued', $6, $7, $8)
         ON CONFLICT (service_instance_id) WHERE status IN ('queued', 'running') DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(instance)
    .bind(deployment)
    .bind(node)
    .bind(trigger.as_str())
    .bind(scheduled_for)
    .bind(timeout as i32)
    .bind(user)
    .fetch_optional(&state.db)
    .await?;
    if queued.is_none() {
        if trigger == Trigger::Manual {
            return Err(AppError::Conflict("a run is already in progress".into()));
        }
        sqlx::query(
            "INSERT INTO cron_runs (id, service_instance_id, deployment_id, trigger, status,
                                    scheduled_for, timeout_secs, finished_at, error)
             VALUES ($1, $2, $3, $4, 'skipped', $5, $6, now(), 'the previous run was still running')",
        )
        .bind(id)
        .bind(instance)
        .bind(deployment)
        .bind(trigger.as_str())
        .bind(scheduled_for)
        .bind(timeout as i32)
        .execute(&state.db)
        .await?;
        tracing::info!(%instance, "cron run skipped: the previous one is still running");
        return Ok(id);
    }
    let fail = |error: &'static str| async move {
        sqlx::query(
            "UPDATE cron_runs SET status = 'failed', error = $2, finished_at = now()
             WHERE id = $1 AND status IN ('queued', 'running')",
        )
        .bind(id)
        .bind(error)
        .execute(&state.db)
        .await
    };
    let workload = match node {
        Some(n) => crate::deploy::desired::cron_workload(state, deployment, n).await?,
        None => None,
    };
    let (Some(node), Some(workload)) = (node, workload) else {
        fail("the deployment's server is gone").await?;
        return Ok(id);
    };
    // Running before it is sent, so a quick result finds it.
    sqlx::query("UPDATE cron_runs SET status = 'running', started_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    let sent = state
        .hub
        .send(
            node,
            pb::server_message::Msg::CronRun(pb::CronRun {
                run_id: id.to_string(),
                workload: Some(Box::new(workload)),
                timeout_secs: timeout,
            }),
        )
        .await;
    if !sent {
        fail("the server running this service is offline").await?;
    } else {
        tracing::info!(%instance, run = %id, %node, "cron run started");
    }
    Ok(id)
}

/// A node's report on a run it was sent; reports on runs another node owns
/// (or that already finished) are ignored.
pub async fn on_result(state: &AppState, node: Id, r: pb::CronResult) -> anyhow::Result<()> {
    let Ok(id) = r.run_id.parse::<Id>() else {
        return Ok(());
    };
    let error: String = r.error.chars().take(2000).collect();
    let status = if r.timed_out {
        "timed_out"
    } else if !error.is_empty() || r.exit_code != 0 {
        "failed"
    } else {
        "succeeded"
    };
    sqlx::query(
        "UPDATE cron_runs SET status = $3, exit_code = $4, error = $5, finished_at = now()
         WHERE id = $1 AND node_id = $2 AND status = 'running'",
    )
    .bind(id)
    .bind(node)
    .bind(status)
    .bind((error.is_empty() && !r.timed_out).then_some(r.exit_code))
    .bind((!error.is_empty()).then_some(&error))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Runs that never reported back time out; old runs are forgotten.
async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let stale = sqlx::query(
        "UPDATE cron_runs SET status = 'timed_out', finished_at = now(),
                error = 'no result from the server running it'
         WHERE status = 'running'
           AND started_at < now() - make_interval(secs => timeout_secs + $1)",
    )
    .bind(RESULT_GRACE_SECS)
    .execute(&state.db)
    .await?;
    if stale.rows_affected() > 0 {
        tracing::warn!(
            runs = stale.rows_affected(),
            "cron runs gave no result in time"
        );
    }
    sqlx::query(
        "UPDATE cron_runs SET status = 'failed', finished_at = now(), error = 'never started'
         WHERE status = 'queued' AND created_at < now() - interval '5 minutes'",
    )
    .execute(&state.db)
    .await?;
    sqlx::query(
        "DELETE FROM cron_runs WHERE status NOT IN ('queued', 'running')
           AND created_at < now() - make_interval(days => $1)",
    )
    .bind(KEEP_DAYS)
    .execute(&state.db)
    .await?;
    Ok(())
}
