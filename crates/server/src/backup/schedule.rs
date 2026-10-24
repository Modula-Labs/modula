//! The periodic backup tick: start due scheduled backups, fail ones that
//! never reported back, and prune what retention no longer keeps.

use std::collections::HashMap;

use modula_id::Id;
use time::OffsetDateTime;

use super::retention::{self, Policy};
use super::run::{Removal, abort, remove, start};
use super::{Kind, control_plane};
use crate::jobs::Job;
use crate::state::AppState;

pub const TICK_JOB: &str = "backups.tick";

/// SQL: whether a daily slot at `hour_utc` is due, given `last_scheduled_at`.
/// Due once per UTC day, as soon as the hour has passed (so a slot missed
/// while the server was down still runs that day).
macro_rules! due {
    () => {
        "now() >= date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                   + make_interval(hours => hour_utc)
         AND (last_scheduled_at IS NULL
              OR last_scheduled_at < date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                                     + make_interval(hours => hour_utc))"
    };
}
pub(crate) use due;

pub async fn tick_job(state: AppState, _job: Job) -> anyhow::Result<()> {
    start_due(&state).await?;
    control_plane::start_due(&state).await?;
    super::pitr::tick(&state).await?;
    sweep_stale(&state).await?;
    prune(&state).await?;
    Ok(())
}

async fn start_due(state: &AppState) -> anyhow::Result<()> {
    // Claiming by update keeps two server replicas from both starting one.
    let due: Vec<(Id, Id)> = sqlx::query_as(concat!(
        "UPDATE backup_schedules SET last_scheduled_at = now() WHERE enabled AND ",
        due!(),
        " RETURNING service_instance_id, target_id"
    ))
    .fetch_all(&state.db)
    .await?;
    for (instance, target) in due {
        if let Err(e) = start(state, Some(instance), target, Kind::Scheduled, None).await {
            tracing::warn!(%instance, error = ?e, "could not start a scheduled backup");
        }
    }
    Ok(())
}

/// Give up on backups and restores that have not reported back within 14
/// hours (the node's own limit is 12): the node died or the result was lost.
async fn sweep_stale(state: &AppState) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    // id, target, object key, upload, service instance.
    type Stale = (Id, Id, String, Option<String>, Option<Id>);
    let stale: Vec<Stale> = sqlx::query_as(
        "UPDATE backups SET status = 'failed', finished_at = now(),
                error = 'no result from the server running the dump'
         WHERE status = 'running' AND started_at < now() - interval '14 hours'
         RETURNING id, target_id, object_key, upload_id, service_instance_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (id, _, _, _, instance) in &stale {
        if let Some(instance) = instance {
            super::run::alert_failed(&mut tx, *id, *instance).await?;
        }
    }
    tx.commit().await?;
    for (id, target, key, upload, _) in stale {
        tracing::warn!(backup = %id, "backup timed out");
        if let Some(upload) = upload {
            abort(state, target, &key, &upload).await;
        }
    }
    sqlx::query(
        "UPDATE backup_restores SET status = 'failed', finished_at = now(),
                error = 'no result from the server running the restore'
         WHERE status = 'running' AND started_at < now() - interval '14 hours'",
    )
    .execute(&state.db)
    .await?;
    // A deletion a node never answered is tried again (by the prune).
    sqlx::query(
        "UPDATE backups SET status = 'completed', deleted_at = NULL
         WHERE status = 'deleting' AND deleted_at < now() - interval '1 hour'",
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Delete scheduled backups no retention rule keeps (manual ones stay until
/// deleted by hand). Failures are retried on the next tick.
async fn prune(state: &AppState) -> anyhow::Result<()> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Id,
        service_instance_id: Option<Id>,
        finished_at: OffsetDateTime,
        #[sqlx(flatten)]
        policy: Policy,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT b.id, b.service_instance_id, b.finished_at,
                coalesce(s.daily_days, $1) AS daily_days,
                coalesce(s.weekly_days, $2) AS weekly_days,
                coalesce(s.monthly_days, $3) AS monthly_days
         FROM backups b LEFT JOIN backup_schedules s ON s.service_instance_id = b.service_instance_id
         WHERE b.status = 'completed' AND b.kind = 'scheduled' AND b.method = 'dump'
           AND b.finished_at IS NOT NULL",
    )
    .bind(retention::DEFAULT.daily_days)
    .bind(retention::DEFAULT.weekly_days)
    .bind(retention::DEFAULT.monthly_days)
    .fetch_all(&state.db)
    .await?;
    // Per service instance (None = the control plane).
    let mut groups: HashMap<Option<Id>, Vec<Row>> = HashMap::new();
    for r in rows {
        groups.entry(r.service_instance_id).or_default().push(r);
    }
    let now = OffsetDateTime::now_utc();
    for rows in groups.values() {
        let backups: Vec<(&Row, OffsetDateTime)> =
            rows.iter().map(|r| (r, r.finished_at)).collect();
        for row in retention::expired(&backups, now, &rows[0].policy) {
            match remove(state, row.id).await {
                Ok(Removal::Done) => tracing::info!(backup = %row.id, "expired backup deleted"),
                Ok(Removal::Requested) => {}
                Err(e) => {
                    tracing::warn!(backup = %row.id, error = ?e, "could not delete an expired backup")
                }
            }
        }
    }
    Ok(())
}
