//! Moving one service instance, volumes' data included, to another server:
//! a step machine over an `instance_moves` row, polled by its own job (a move
//! on its own) or by a server migration (one move per instance, in turn).
//!
//! stop → stopping → copy → deploy → wait → done:
//!
//! - stop: a service with a volume stops on the old server (its serving
//!   deployment is removed); one without keeps serving until its new
//!   deployment is healthy.
//! - stopping: until the old server applied that desired state (the container
//!   is gone, so the data is at rest).
//! - copy: each volume's data, old server → control plane → new server (see
//!   [`super::transfer`]); retried twice.
//! - deploy: the instance's server and TCP proxy switch to the new one and the
//!   serving deployment is deployed again there (same image, or the same
//!   commit or upload rebuilt).
//! - wait: until that deployment is active; activation moves custom domains'
//!   DNS records (deploy/state.rs).
//!
//! Failure stops the machine with the old server's data untouched (the copy
//! only reads it). [`restore`] undoes a move: the instance goes back to its
//! old server and its previous deployment runs there again; data written on
//! the new server since is not copied back.

use std::time::Duration;

use modula_id::Id;
use serde_json::json;
use time::OffsetDateTime;

use super::transfer::CopyState;
use crate::deploy::config::ServiceConfig;
use crate::deploy::start::{
    BUILD_PREFIX, LocalGit, NewDeployment, Prebuilt, Started, Trigger, start,
};
use crate::deploy::{self, LiveFilter};
use crate::error::{AppError, AppResult};
use crate::github::Commit;
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

pub const MOVE_JOB: &str = "migrate.move";
const POLL: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: time::Duration = time::Duration::minutes(10);
/// Waiting for both servers to be connected before a copy.
const CONNECT_TIMEOUT: time::Duration = time::Duration::minutes(10);
/// Building from source again can take a while.
const HEALTHY_TIMEOUT: time::Duration = time::Duration::minutes(45);
const COPY_ATTEMPTS: i32 = 3;

/// Where a move stands after a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Pending,
    Done,
    Failed(String),
    /// Aborted, or not running for another reason.
    Stopped,
}

pub async fn enqueue(state: &AppState, id: Id, after: Duration) -> AppResult<()> {
    jobs::enqueue(
        &state.db,
        NewJob {
            kind: MOVE_JOB,
            payload: json!({ "id": id }),
            run_at: Some(OffsetDateTime::now_utc() + after),
            max_attempts: Some(5),
            dedupe_key: None,
        },
    )
    .await?;
    state.jobs_notify.notify_one();
    Ok(())
}

/// A move on its own (a server migration polls its moves itself).
pub async fn move_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let id: Id = serde_json::from_value(job.payload["id"].clone())?;
    let p = poll(&state, id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if p == Poll::Pending {
        enqueue(&state, id, POLL)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(())
}

pub struct NewMove {
    pub workspace: Id,
    pub instance: Id,
    pub migration: Option<Id>,
    /// `primary` or `replicas`.
    pub kind: &'static str,
    pub from: Option<Id>,
    pub to: Id,
    pub created_by: Option<Id>,
    /// `running`, or `pending` for a server migration's later moves.
    pub status: &'static str,
}

pub async fn create(db: impl sqlx::PgExecutor<'_>, m: NewMove) -> AppResult<Id> {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO instance_moves (id, workspace_id, service_instance_id, migration_id, kind,
                                     from_node_id, to_node_id, status, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(m.workspace)
    .bind(m.instance)
    .bind(m.migration)
    .bind(m.kind)
    .bind(m.from)
    .bind(m.to)
    .bind(m.status)
    .bind(m.created_by)
    .execute(db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(d) if d.constraint() == Some("instance_moves_active_key") => {
            AppError::Conflict("this service is already being moved".into())
        }
        e => e.into(),
    })?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct Row {
    service_instance_id: Id,
    kind: String,
    from_node_id: Option<Id>,
    to_node_id: Option<Id>,
    status: String,
    phase: String,
    phase_at: OffsetDateTime,
    previous_deployment_id: Option<Id>,
    deployment_id: Option<Id>,
    stop_generation: Option<i64>,
    copy_attempts: i32,
    created_by: Option<Id>,
    /// "web (production)"
    label: String,
    from_name: Option<String>,
    to_name: Option<String>,
}

async fn load(state: &AppState, id: Id) -> AppResult<Row> {
    sqlx::query_as(
        "SELECT m.service_instance_id, m.kind, m.from_node_id, m.to_node_id, m.status, m.phase,
                m.phase_at, m.previous_deployment_id, m.deployment_id, m.stop_generation,
                m.copy_attempts, m.created_by,
                s.name || ' (' || e.name || ')' AS label, f.name AS from_name, t.name AS to_name
         FROM instance_moves m
         JOIN service_instances i ON i.id = m.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         LEFT JOIN nodes f ON f.id = m.from_node_id
         LEFT JOIN nodes t ON t.id = m.to_node_id
         WHERE m.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Append a running step to `table`'s progress list.
pub(super) async fn begin(
    db: impl sqlx::PgExecutor<'_>,
    table: Table,
    id: Id,
    label: &str,
) -> AppResult<()> {
    sqlx::query(table.begin_sql())
        .bind(id)
        .bind(label)
        .execute(db)
        .await?;
    Ok(())
}

/// Settle the last step if it is still running.
pub(super) async fn end(
    db: impl sqlx::PgExecutor<'_>,
    table: Table,
    id: Id,
    status: &str,
    detail: Option<&str>,
) -> AppResult<()> {
    sqlx::query(table.end_sql())
        .bind(id)
        .bind(status)
        .bind(detail)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Table {
    Moves,
    Migrations,
}

macro_rules! begin_sql {
    ($table:literal) => {
        concat!(
            "UPDATE ",
            $table,
            " SET steps = steps || jsonb_build_array(jsonb_build_object('label', $2::text, 'status', 'running')),
                  updated_at = now()
              WHERE id = $1"
        )
    };
}

macro_rules! end_sql {
    ($table:literal) => {
        concat!(
            "UPDATE ",
            $table,
            " SET steps = jsonb_set(steps, ARRAY[(jsonb_array_length(steps) - 1)::text],
                      (steps -> -1) || jsonb_strip_nulls(jsonb_build_object('status', $2::text, 'detail', $3::text))),
                  updated_at = now()
              WHERE id = $1 AND steps -> -1 ->> 'status' = 'running'"
        )
    };
}

impl Table {
    fn begin_sql(self) -> &'static str {
        match self {
            Self::Moves => begin_sql!("instance_moves"),
            Self::Migrations => begin_sql!("server_migrations"),
        }
    }

    fn end_sql(self) -> &'static str {
        match self {
            Self::Moves => end_sql!("instance_moves"),
            Self::Migrations => end_sql!("server_migrations"),
        }
    }
}

async fn set_phase(db: impl sqlx::PgExecutor<'_>, id: Id, phase: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE instance_moves SET phase = $2, phase_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(phase)
    .execute(db)
    .await?;
    Ok(())
}

async fn fail(state: &AppState, id: Id, error: &str) -> AppResult<Poll> {
    end(&state.db, Table::Moves, id, "failed", Some(error)).await?;
    sqlx::query(
        "UPDATE instance_moves SET status = 'failed', error = $2, updated_at = now(), finished_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(error)
    .execute(&state.db)
    .await?;
    Ok(Poll::Failed(error.to_owned()))
}

/// The deployment serving an instance (or rolling out, when none serves).
async fn serving(db: impl sqlx::PgExecutor<'_>, instance: Id) -> AppResult<Option<Id>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM deployments
         WHERE service_instance_id = $1
           AND status IN ('active', 'sleeping', 'crashed', 'deploying', 'building', 'queued', 'waiting_ci')
         ORDER BY status IN ('active', 'sleeping', 'crashed') DESC, created_at DESC LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(db)
    .await?)
}

async fn volumes(state: &AppState, instance: Id) -> AppResult<Vec<Id>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM volumes WHERE service_instance_id = $1 ORDER BY id")
            .bind(instance)
            .fetch_all(&state.db)
            .await?,
    )
}

fn elapsed(since: OffsetDateTime) -> time::Duration {
    OffsetDateTime::now_utc() - since
}

/// One step of move `id`.
pub async fn poll(state: &AppState, id: Id) -> AppResult<Poll> {
    let r = load(state, id).await?;
    match r.status.as_str() {
        "running" => {}
        "done" => return Ok(Poll::Done),
        "failed" => return Ok(Poll::Failed(String::new())),
        _ => return Ok(Poll::Stopped),
    }
    let Some(to) = r.to_node_id else {
        return fail(state, id, "the new server was removed").await;
    };
    let to_name = r.to_name.clone().unwrap_or_default();
    let instance = r.service_instance_id;
    match r.phase.as_str() {
        "stop" => {
            let vols = volumes(state, instance).await?;
            let mut tx = state.db.begin().await?;
            let previous = serving(&mut *tx, instance).await?;
            sqlx::query("UPDATE instance_moves SET previous_deployment_id = $2 WHERE id = $1")
                .bind(id)
                .bind(previous)
                .execute(&mut *tx)
                .await?;
            let stop = r.kind == "primary" && !vols.is_empty() && r.from_node_id.is_some();
            if !stop {
                let next = if r.kind == "primary" && !vols.is_empty() {
                    "copy"
                } else {
                    "deploy"
                };
                set_phase(&mut *tx, id, next).await?;
                tx.commit().await?;
                return Ok(Poll::Pending);
            }
            let from = r.from_node_id.expect("checked");
            begin(
                &mut *tx,
                Table::Moves,
                id,
                &format!(
                    "Stopping {} on {}",
                    r.label,
                    r.from_name.as_deref().unwrap_or("the old server")
                ),
            )
            .await?;
            // Every live deployment: none may write the data during the copy.
            let live: Vec<Id> = sqlx::query_scalar(
                "SELECT id FROM deployments WHERE service_instance_id = $1
                 AND status IN ('waiting_ci', 'queued', 'building', 'deploying', 'active', 'sleeping', 'crashed')",
            )
            .bind(instance)
            .fetch_all(&mut *tx)
            .await?;
            let mut kick = Vec::new();
            for d in live {
                kick.extend(deploy::remove_live(&mut tx, LiveFilter::Deployment, d).await?);
            }
            deploy::bump(&mut *tx, from).await?;
            kick.push(from);
            let generation: i64 =
                sqlx::query_scalar("SELECT desired_generation FROM nodes WHERE id = $1")
                    .bind(from)
                    .fetch_one(&mut *tx)
                    .await?;
            sqlx::query("UPDATE instance_moves SET stop_generation = $2 WHERE id = $1")
                .bind(id)
                .bind(generation)
                .execute(&mut *tx)
                .await?;
            set_phase(&mut *tx, id, "stopping").await?;
            tx.commit().await?;
            state.hub.kick_all(&kick);
            Ok(Poll::Pending)
        }
        "stopping" => {
            let from = r
                .from_node_id
                .ok_or_else(|| AppError::Conflict("no old server".into()));
            let applied: Option<i64> = match from {
                Ok(n) => {
                    sqlx::query_scalar("SELECT applied_generation FROM nodes WHERE id = $1")
                        .bind(n)
                        .fetch_optional(&state.db)
                        .await?
                }
                Err(_) => None,
            };
            if applied.is_some_and(|a| a >= r.stop_generation.unwrap_or(i64::MAX)) {
                end(&state.db, Table::Moves, id, "done", None).await?;
                set_phase(&state.db, id, "copy").await?;
            } else if elapsed(r.phase_at) > STOP_TIMEOUT {
                return fail(
                    state,
                    id,
                    "the old server did not stop the service within 10 minutes; is it online?",
                )
                .await;
            }
            Ok(Poll::Pending)
        }
        "copy" => {
            let vols = volumes(state, instance).await?;
            let Some(from) = r.from_node_id.filter(|_| !vols.is_empty()) else {
                set_phase(&state.db, id, "deploy").await?;
                return Ok(Poll::Pending);
            };
            let retry = match state.transfers.copy_result(id) {
                CopyState::Running => return Ok(Poll::Pending),
                CopyState::Done(Ok(bytes)) => {
                    end(
                        &state.db,
                        Table::Moves,
                        id,
                        "done",
                        Some(&human_bytes(bytes)),
                    )
                    .await?;
                    sqlx::query("UPDATE instance_moves SET copied_bytes = $2 WHERE id = $1")
                        .bind(id)
                        .bind(i64::try_from(bytes).unwrap_or(i64::MAX))
                        .execute(&state.db)
                        .await?;
                    set_phase(&state.db, id, "deploy").await?;
                    return Ok(Poll::Pending);
                }
                CopyState::Done(Err(e)) => Some(e),
                CopyState::Missing => None,
            };
            if r.copy_attempts >= COPY_ATTEMPTS {
                let why = retry.unwrap_or_else(|| "the copy was interrupted".into());
                return fail(state, id, &format!("copying the data failed: {why}")).await;
            }
            if !(state.hub.is_connected(from) && state.hub.is_connected(to)) {
                if elapsed(r.phase_at) > CONNECT_TIMEOUT {
                    return fail(
                        state,
                        id,
                        "both servers must be online to copy the data; one stayed offline",
                    )
                    .await;
                }
                return Ok(Poll::Pending);
            }
            if r.copy_attempts == 0 {
                let label = match vols.len() {
                    1 => "Copying the volume's data".to_owned(),
                    n => format!("Copying {n} volumes' data"),
                };
                begin(
                    &state.db,
                    Table::Moves,
                    id,
                    &format!("{label} to {to_name}"),
                )
                .await?;
            }
            sqlx::query(
                "UPDATE instance_moves SET copy_attempts = copy_attempts + 1 WHERE id = $1",
            )
            .bind(id)
            .execute(&state.db)
            .await?;
            state.transfers.start_copy(state, id, from, to, vols);
            Ok(Poll::Pending)
        }
        "deploy" => {
            begin(
                &state.db,
                Table::Moves,
                id,
                &format!("Starting on {to_name}"),
            )
            .await?;
            let mut tx = state.db.begin().await?;
            let started = async {
                if r.kind == "primary" {
                    switch(&mut tx, instance, to).await?;
                }
                let source = match r.previous_deployment_id {
                    Some(d) => Some(d),
                    None => serving(&mut *tx, instance).await?,
                };
                match source {
                    Some(d) => redeploy(state, &mut tx, instance, d, r.created_by).await,
                    None => Ok(None),
                }
            }
            .await;
            let started = match started {
                Ok(s) => s,
                Err(e) => {
                    drop(tx);
                    return fail(state, id, &error_text(&e)).await;
                }
            };
            sqlx::query("UPDATE instance_moves SET deployment_id = $2 WHERE id = $1")
                .bind(id)
                .bind(started.as_ref().map(|s| s.id))
                .execute(&mut *tx)
                .await?;
            set_phase(
                &mut *tx,
                id,
                if started.is_some() { "wait" } else { "done" },
            )
            .await?;
            tx.commit().await?;
            if let Some(s) = &started {
                state.hub.kick_all(&s.kick);
                Ok(Poll::Pending)
            } else {
                finish(state, id, to).await
            }
        }
        "wait" => {
            let Some(d) = r.deployment_id else {
                return fail(state, id, "the new deployment is gone").await;
            };
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1")
                    .bind(d)
                    .fetch_optional(&state.db)
                    .await?;
            match status.as_deref() {
                Some("active" | "sleeping") => finish(state, id, to).await,
                Some(s @ ("failed" | "crashed" | "cancelled" | "removed")) => {
                    fail(state, id, &format!("the deployment on {to_name} {s}")).await
                }
                None => fail(state, id, "the new deployment is gone").await,
                Some(_) if elapsed(r.phase_at) > HEALTHY_TIMEOUT => {
                    fail(
                        state,
                        id,
                        &format!("the deployment on {to_name} was not healthy within 45 minutes"),
                    )
                    .await
                }
                Some(_) => Ok(Poll::Pending),
            }
        }
        _ => Ok(Poll::Done),
    }
}

/// 12.3 MB
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// What a user may see of an error (internal ones are only logged).
pub fn error_text(e: &AppError) -> String {
    match e {
        AppError::Internal(err) => {
            tracing::warn!(error = ?err, "moving a service failed");
            "an internal error; see the control plane's log".into()
        }
        e => e.to_string(),
    }
}

async fn finish(state: &AppState, id: Id, to: Id) -> AppResult<Poll> {
    end(&state.db, Table::Moves, id, "done", None).await?;
    sqlx::query(
        "UPDATE instance_moves SET status = 'done', phase = 'done', updated_at = now(), finished_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    // Activation already resynced its custom domains; this catches anything
    // else served from the new server.
    crate::domains::resync_for_node(&state.db, to, None).await?;
    state.jobs_notify.notify_one();
    Ok(Poll::Done)
}

/// Point the instance and its TCP proxy at `node`. The proxy keeps its public
/// port when that is free there, else takes the first free one.
async fn switch(tx: &mut sqlx::PgConnection, instance: Id, node: Id) -> AppResult<()> {
    sqlx::query("UPDATE service_instances SET node_id = $2, updated_at = now() WHERE id = $1")
        .bind(instance)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let proxy: Option<(Id, i32, Id)> = sqlx::query_as(
        "SELECT id, public_port, node_id FROM tcp_proxies WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((proxy, port, on)) = proxy else {
        return Ok(());
    };
    if on == node {
        return Ok(());
    }
    // Serialized per node like a new proxy's port (api/storage.rs).
    sqlx::query("SELECT 1 FROM nodes WHERE id = $1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let used: Vec<i32> =
        sqlx::query_scalar("SELECT public_port FROM tcp_proxies WHERE node_id = $1")
            .bind(node)
            .fetch_all(&mut *tx)
            .await?;
    let port = std::iter::once(port)
        .chain(20000..=29999)
        .find(|p| !used.contains(p))
        .ok_or_else(|| AppError::Conflict("the new server has no free TCP proxy ports".into()))?;
    sqlx::query("UPDATE tcp_proxies SET node_id = $2, public_port = $3 WHERE id = $1")
        .bind(proxy)
        .bind(node)
        .bind(port)
        .execute(&mut *tx)
        .await?;
    deploy::bump(&mut *tx, on).await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Source {
    environment_id: Id,
    config: serde_json::Value,
    image: String,
    image_digest: Option<String>,
    commit_sha: Option<String>,
    commit_message: Option<String>,
    commit_author: Option<String>,
    commit_author_login: Option<String>,
    source_branch: Option<String>,
    source_dirty: Option<bool>,
    upload_kept: bool,
}

/// Deploy what `source` ran again, wherever the instance is placed now: the
/// same image, or (built from source) the same commit or kept upload rebuilt,
/// since built images stay on the server that built them. Instances served
/// by Cloudflare don't run on a server: nothing to deploy.
async fn redeploy(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    instance: Id,
    source: Id,
    created_by: Option<Id>,
) -> AppResult<Option<Started>> {
    let s: Source = sqlx::query_as(
        "SELECT i.environment_id, d.config, d.image, d.image_digest, d.commit_sha, d.commit_message,
                d.commit_author, d.commit_author_login, d.source_branch, d.source_dirty,
                u.id IS NOT NULL AND u.purged_at IS NULL AS upload_kept
         FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id
         LEFT JOIN source_uploads u ON u.id = d.upload_id
         WHERE d.id = $1",
    )
    .bind(source)
    .fetch_one(&mut *tx)
    .await?;
    let cfg: ServiceConfig = serde_json::from_value(s.config).map_err(anyhow::Error::from)?;
    if cfg.cloudflare().is_some() {
        return Ok(None);
    }
    let (commit, local_git, prebuilt) = if s.image.starts_with(BUILD_PREFIX) {
        if cfg.upload().is_some() {
            if !s.upload_kept {
                return Err(AppError::Conflict(
                    "its uploaded files are no longer kept, so it can't be built on the new server; run `modula up` again".into(),
                ));
            }
            let git = LocalGit {
                commit: s.commit_sha,
                message: s.commit_message,
                branch: s.source_branch,
                dirty: s.source_dirty,
            };
            (None, Some(git), None)
        } else {
            let commit = s.commit_sha.map(|sha| Commit {
                sha,
                message: s.commit_message.unwrap_or_default(),
                author: s.commit_author.unwrap_or_default(),
                author_login: s.commit_author_login,
            });
            if cfg.repo().is_some() != commit.is_some() {
                return Err(AppError::Conflict(
                    "its source can't be built again on the new server".into(),
                ));
            }
            (commit, None, None)
        }
    } else {
        let p = Prebuilt {
            image: s.image,
            digest: s.image_digest,
        };
        (None, None, Some(p))
    };
    let started = start(
        state,
        &mut *tx,
        s.environment_id,
        NewDeployment {
            instance,
            config: cfg,
            trigger: Trigger::Redeploy,
            created_by,
            commit,
            local_git,
            prebuilt,
            source_deployment: Some(source),
        },
    )
    .await?;
    Ok(Some(started))
}

/// Undo move `id` (running, failed, or with `include_done` also finished):
/// the instance goes back to its old server and the deployment that served
/// before the move runs there again. Returns false when there was nothing to
/// undo.
pub async fn restore(state: &AppState, id: Id, include_done: bool) -> AppResult<bool> {
    let claimed = sqlx::query(
        "UPDATE instance_moves SET status = 'aborting', updated_at = now()
         WHERE id = $1 AND (status IN ('running', 'failed') OR ($2 AND status = 'done'))",
    )
    .bind(id)
    .bind(include_done)
    .execute(&state.db)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(false);
    }
    state.transfers.cancel_copy(id);
    if let Err(e) = undo(state, id).await {
        let why = format!("undoing the move failed: {}", error_text(&e));
        sqlx::query(
            "UPDATE instance_moves SET status = 'failed', error = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&why)
        .execute(&state.db)
        .await?;
        return Err(AppError::Conflict(why));
    }
    Ok(true)
}

async fn undo(state: &AppState, id: Id) -> AppResult<()> {
    let r = load(state, id).await?;
    let from_name = r
        .from_name
        .clone()
        .unwrap_or_else(|| "the old server".into());
    let mut tx = state.db.begin().await?;
    let mut kick = Vec::new();
    let switched = matches!(r.phase.as_str(), "deploy" | "wait" | "done");
    if r.phase != "stop" {
        if switched
            && r.kind == "primary"
            && let Some(from) = r.from_node_id
        {
            switch(&mut tx, r.service_instance_id, from).await?;
            kick.push(from);
            if let Some(to) = r.to_node_id {
                kick.push(to);
            }
        }
        let back = match r.previous_deployment_id {
            Some(prev) => {
                redeploy(state, &mut tx, r.service_instance_id, prev, r.created_by).await?
            }
            None => None,
        };
        if let Some(s) = back {
            kick.extend(s.kick);
        } else if let Some(d) = r.deployment_id {
            kick.extend(deploy::remove_live(&mut tx, LiveFilter::Deployment, d).await?);
        }
    }
    end(&mut *tx, Table::Moves, id, "failed", Some("stopped")).await?;
    begin(&mut *tx, Table::Moves, id, &format!("Back on {from_name}")).await?;
    end(&mut *tx, Table::Moves, id, "done", None).await?;
    sqlx::query(
        "UPDATE instance_moves SET status = 'aborted', updated_at = now(), finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    kick.sort();
    kick.dedup();
    state.hub.kick_all(&kick);
    Ok(())
}
