//! Durable Postgres job queue. Jobs are claimed with `FOR UPDATE SKIP LOCKED`
//! under a lease; a crashed worker's jobs return to the queue when the lease expires.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::error::AppResult;
use crate::state::AppState;

pub const LEASE: Duration = Duration::from_secs(300);
const IDLE_POLL: Duration = Duration::from_secs(2);
const MAX_BACKOFF_SECS: i64 = 600;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
}

#[derive(Debug, Default)]
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub payload: Value,
    pub run_at: Option<OffsetDateTime>,
    pub max_attempts: Option<i32>,
    /// At most one queued/running job per key; extra enqueues are dropped.
    pub dedupe_key: Option<&'a str>,
}

/// Returns the new job id, or None when deduplicated.
pub async fn enqueue(db: &PgPool, job: NewJob<'_>) -> AppResult<Option<i64>> {
    let id = sqlx::query_scalar(
        "INSERT INTO jobs (kind, payload, run_at, max_attempts, dedupe_key)
         VALUES ($1, $2, COALESCE($3, now()), COALESCE($4, 5), $5)
         ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL AND status IN ('queued', 'running')
         DO NOTHING
         RETURNING id",
    )
    .bind(job.kind)
    .bind(job.payload)
    .bind(job.run_at)
    .bind(job.max_attempts)
    .bind(job.dedupe_key)
    .fetch_optional(db)
    .await?;
    Ok(id)
}

pub async fn claim(db: &PgPool, worker: &str) -> AppResult<Option<Job>> {
    let job = sqlx::query_as(
        "UPDATE jobs SET status = 'running', locked_by = $1,
                locked_until = now() + make_interval(secs => $2),
                attempts = attempts + 1, updated_at = now()
         WHERE id = (
             SELECT id FROM jobs
             WHERE status = 'queued' AND run_at <= now()
             ORDER BY run_at, id
             FOR UPDATE SKIP LOCKED
             LIMIT 1)
         RETURNING id, kind, payload, attempts, max_attempts",
    )
    .bind(worker)
    .bind(LEASE.as_secs_f64())
    .fetch_optional(db)
    .await?;
    Ok(job)
}

/// Only the lease holder can complete or fail a job (a reaped job may have moved on).
pub async fn complete(db: &PgPool, id: i64, worker: &str) -> AppResult<bool> {
    let res = sqlx::query(
        "UPDATE jobs SET status = 'succeeded', locked_by = NULL, locked_until = NULL,
                finished_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'running' AND locked_by = $2",
    )
    .bind(id)
    .bind(worker)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub fn backoff_secs(attempts: i32) -> i64 {
    2i64.saturating_pow(attempts.clamp(0, 30) as u32)
        .min(MAX_BACKOFF_SECS)
}

pub async fn fail(
    db: &PgPool,
    job: &Job,
    worker: &str,
    error: &str,
    permanent: bool,
) -> AppResult<()> {
    let dead = permanent || job.attempts >= job.max_attempts;
    let error: String = error.chars().take(4000).collect();
    sqlx::query(
        "UPDATE jobs SET status = CASE WHEN $3 THEN 'dead' ELSE 'queued' END,
                run_at = now() + make_interval(secs => $4),
                last_error = $5, locked_by = NULL, locked_until = NULL, updated_at = now(),
                finished_at = CASE WHEN $3 THEN now() ELSE NULL END
         WHERE id = $1 AND status = 'running' AND locked_by = $2",
    )
    .bind(job.id)
    .bind(worker)
    .bind(dead)
    .bind(backoff_secs(job.attempts) as f64)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Requeue jobs whose worker vanished. Attempts already counted at claim time.
pub async fn reap_expired(db: &PgPool) -> AppResult<u64> {
    let res = sqlx::query(
        "UPDATE jobs SET status = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END,
                last_error = 'lease expired', locked_by = NULL, locked_until = NULL, updated_at = now()
         WHERE status = 'running' AND locked_until < now()",
    )
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

pub type Handler =
    Arc<dyn Fn(AppState, Job) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

#[derive(Default, Clone)]
pub struct Registry(HashMap<&'static str, Handler>);

impl Registry {
    pub fn on<F, Fut>(mut self, kind: &'static str, f: F) -> Self
    where
        F: Fn(AppState, Job) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.0.insert(kind, Arc::new(move |s, j| Box::pin(f(s, j))));
        self
    }
}

/// Run until `stop` is cancelled. One job at a time per worker; spawn more for parallelism.
pub async fn run_worker(
    state: AppState,
    registry: Registry,
    worker: String,
    stop: CancellationToken,
) {
    let mut last_reap = std::time::Instant::now() - Duration::from_secs(60);
    loop {
        if stop.is_cancelled() {
            return;
        }
        if last_reap.elapsed() > Duration::from_secs(30) {
            if let Err(e) = reap_expired(&state.db).await {
                tracing::warn!(error = %e, "job reaper failed");
            }
            last_reap = std::time::Instant::now();
        }
        let job = match claim(&state.db, &worker).await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "job claim failed");
                None
            }
        };
        let Some(job) = job else {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = state.jobs_notify.notified() => {}
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
            continue;
        };
        let span =
            tracing::info_span!("job", id = job.id, kind = %job.kind, attempt = job.attempts);
        // Instrument instead of `span.enter()`: a guard held across `.await` leaks
        // the span into unrelated tasks on the same runtime thread.
        async {
            let outcome = match registry.0.get(job.kind.as_str()) {
                None => Err((format!("no handler for job kind {}", job.kind), true)),
                Some(h) => h(state.clone(), job.clone())
                    .await
                    .map_err(|e| (format!("{e:#}"), false)),
            };
            let res = match outcome {
                Ok(()) => complete(&state.db, job.id, &worker).await.map(|_| ()),
                Err((msg, permanent)) => {
                    tracing::warn!(error = %msg, "job failed");
                    fail(&state.db, &job, &worker, &msg, permanent).await
                }
            };
            if let Err(e) = res {
                tracing::error!(error = %e, "could not record job outcome");
            }
        }
        .instrument(span)
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::backoff_secs;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(20), 600);
        assert_eq!(backoff_secs(i32::MAX), 600);
    }
}
