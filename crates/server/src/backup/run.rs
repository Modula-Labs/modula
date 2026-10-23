//! Running one backup or restore: dispatch to the node, collect its result,
//! complete (or abort) the multipart upload.

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::{Engine, Kind, PART_URLS, load_target, part_size, stamp};
use crate::error::{AppError, AppResult};
use crate::jobs::{self, Job};
use crate::state::AppState;

pub const RUN_JOB: &str = "backup.run";
pub const FINISH_JOB: &str = "backup.finish";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunPayload {
    backup_id: Id,
}

/// The engine of a database service instance and its workspace.
pub async fn instance_engine(state: &AppState, instance: Id) -> AppResult<(Engine, Id)> {
    let (template, workspace): (Option<String>, Id) = sqlx::query_as(
        "SELECT s.template, p.workspace_id FROM service_instances i
         JOIN services s ON s.id = i.service_id JOIN projects p ON p.id = s.project_id
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    let engine = template.as_deref().and_then(Engine::parse).ok_or_else(|| {
        AppError::Conflict("only database services (from a template) can be backed up".into())
    })?;
    Ok((engine, workspace))
}

/// Record a running backup of `instance` (None = the control plane's own
/// database) and queue its dispatch. Conflict if one is already running.
pub async fn start(
    state: &AppState,
    instance: Option<Id>,
    target: Id,
    kind: Kind,
    created_by: Option<Id>,
) -> AppResult<Id> {
    let id = Id::new();
    let (engine, name) = match instance {
        Some(instance) => {
            let (engine, workspace) = instance_engine(state, instance).await?;
            let restoring: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM backup_restores
                                WHERE service_instance_id = $1 AND status = 'running')",
            )
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
            if restoring {
                return Err(AppError::Conflict("a restore is running".into()));
            }
            let name = format!(
                "{workspace}/{instance}/{}-{id}.{}.age",
                stamp(OffsetDateTime::now_utc()),
                engine.extension()
            );
            (engine, name)
        }
        None => (
            Engine::Postgres,
            format!(
                "modula-control-plane/{}-{id}.dump.age",
                stamp(OffsetDateTime::now_utc())
            ),
        ),
    };
    let prefix: String = sqlx::query_scalar("SELECT prefix FROM backup_targets WHERE id = $1")
        .bind(target)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::bad_request("unknown backup target"))?;
    let inserted: Option<Id> = sqlx::query_scalar(
        "INSERT INTO backups (id, service_instance_id, target_id, kind, engine, object_key, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(instance)
    .bind(target)
    .bind(kind.as_str())
    .bind(engine.as_str())
    .bind(format!("{prefix}{name}"))
    .bind(created_by)
    .fetch_optional(&state.db)
    .await?;
    if inserted.is_none() {
        return Err(AppError::Conflict("a backup is already running".into()));
    }
    jobs::enqueue(
        &state.db,
        jobs::NewJob {
            kind: RUN_JOB,
            payload: json!(RunPayload { backup_id: id }),
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await?;
    state.jobs_notify.notify_one();
    Ok(id)
}

/// Job: start the upload and hand the backup to the node running the
/// database (or run it here, for the control plane). Failures are recorded
/// on the backup, not retried.
pub async fn run_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let p: RunPayload = serde_json::from_value(job.payload)?;
    let row: Option<(Option<Id>, Id, String, String)> = sqlx::query_as(
        "SELECT service_instance_id, target_id, engine, object_key FROM backups
         WHERE id = $1 AND status = 'running'",
    )
    .bind(p.backup_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((instance, target, engine, key)) = row else {
        return Ok(());
    };
    let Some(instance) = instance else {
        // Long-running: off the job worker, which runs one job at a time.
        tokio::spawn(super::control_plane::run(state.clone(), p.backup_id));
        return Ok(());
    };
    let engine = Engine::parse(&engine).ok_or_else(|| anyhow::anyhow!("unknown engine"))?;
    if let Err(e) = dispatch(&state, p.backup_id, instance, target, engine, &key).await {
        fail(&state, p.backup_id, &format!("{e:#}")).await?;
    }
    Ok(())
}

/// The node running an instance's active deployment, if it can take backup
/// and restore requests right now.
async fn live_node(state: &AppState, instance: Id) -> anyhow::Result<(Id, Id)> {
    let live: Option<(Id, Option<Id>)> = sqlx::query_as(
        "SELECT id, node_id FROM deployments WHERE service_instance_id = $1 AND status = 'active'",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let Some((deployment, Some(node))) = live else {
        anyhow::bail!("the database is not running");
    };
    anyhow::ensure!(
        state.hub.is_connected(node),
        "the server running the database is offline"
    );
    let capable: bool = sqlx::query_scalar(
        "SELECT coalesce(info->'capabilities' ? 'backup', false) FROM nodes WHERE id = $1",
    )
    .bind(node)
    .fetch_one(&state.db)
    .await?;
    anyhow::ensure!(
        capable,
        "the server running the database needs a newer modula-node for backups"
    );
    Ok((deployment, node))
}

async fn dispatch(
    state: &AppState,
    backup: Id,
    instance: Id,
    target_id: Id,
    engine: Engine,
    key: &str,
) -> anyhow::Result<()> {
    let (deployment, node) = live_node(state, instance).await?;
    let (workspace, target) = load_target(state, target_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let recipient = super::recipient(state, workspace)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let previous: Option<i64> = sqlx::query_scalar(
        "SELECT size_bytes FROM backups WHERE service_instance_id = $1 AND status = 'completed'
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let upload_id = target.create_multipart(key).await?;
    // Recorded first, so a failure from here on aborts the upload.
    sqlx::query("UPDATE backups SET upload_id = $2, node_id = $3 WHERE id = $1")
        .bind(backup)
        .bind(&upload_id)
        .bind(node)
        .execute(&state.db)
        .await?;
    let request = pb::BackupRequest {
        backup_id: backup.to_string(),
        deployment_id: deployment.to_string(),
        engine: engine.proto() as i32,
        age_recipient: recipient,
        part_urls: target.presign_parts(key, &upload_id, PART_URLS)?,
        part_size: part_size(previous),
    };
    anyhow::ensure!(
        state
            .hub
            .send(node, pb::server_message::Msg::Backup(request))
            .await,
        "the server running the database is offline"
    );
    Ok(())
}

/// Mark a running backup failed and abort its upload (best effort).
pub async fn fail(state: &AppState, backup: Id, error: &str) -> anyhow::Result<()> {
    let row: Option<(Id, String, Option<String>)> = sqlx::query_as(
        "UPDATE backups SET status = 'failed', error = $2, finished_at = now()
         WHERE id = $1 AND status = 'running'
         RETURNING target_id, object_key, upload_id",
    )
    .bind(backup)
    .bind(error.chars().take(2000).collect::<String>())
    .fetch_optional(&state.db)
    .await?;
    tracing::warn!(%backup, error, "backup failed");
    if let Some((target, key, Some(upload_id))) = row {
        abort(state, target, &key, &upload_id).await;
    }
    Ok(())
}

pub async fn abort(state: &AppState, target: Id, key: &str, upload_id: &str) {
    let aborted = match load_target(state, target).await {
        Ok((_, t)) => t.abort_multipart(key, upload_id).await,
        Err(e) => Err(anyhow::anyhow!("{e:?}")),
    };
    if let Err(e) = aborted {
        tracing::warn!(%target, error = format!("{e:#}"), "could not abort a backup upload");
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FinishPayload {
    backup_id: Id,
    ok: bool,
    size_bytes: u64,
    error: String,
    etags: Vec<String>,
}

/// A backup or restore result from a node. Only the node a request was sent
/// to may report on it.
pub async fn on_result(state: &AppState, node: Id, r: pb::BackupResult) -> anyhow::Result<()> {
    let Ok(id) = r.id.parse::<Id>() else {
        return Ok(());
    };
    let error: String = r.error.chars().take(2000).collect();
    if r.restore {
        sqlx::query(
            "UPDATE backup_restores
             SET status = CASE WHEN $3 THEN 'completed' ELSE 'failed' END,
                 error = CASE WHEN $3 THEN NULL ELSE $4 END, finished_at = now()
             WHERE id = $1 AND node_id = $2 AND status = 'running'",
        )
        .bind(id)
        .bind(node)
        .bind(r.ok)
        .bind(&error)
        .execute(&state.db)
        .await?;
        return Ok(());
    }
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backups WHERE id = $1 AND node_id = $2 AND status = 'running')",
    )
    .bind(id)
    .bind(node)
    .fetch_one(&state.db)
    .await?;
    if !owned {
        return Ok(());
    }
    // Completing the upload is a network call: not on the node's reader.
    let payload = FinishPayload {
        backup_id: id,
        ok: r.ok,
        size_bytes: r.size_bytes,
        error,
        etags: r.etags,
    };
    let dedupe = format!("{FINISH_JOB}:{id}");
    jobs::enqueue(
        &state.db,
        jobs::NewJob {
            kind: FINISH_JOB,
            payload: json!(payload),
            dedupe_key: Some(&dedupe),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    state.jobs_notify.notify_one();
    Ok(())
}

/// Job: complete the multipart upload of a backup the node finished (or
/// abort it), and record the outcome.
pub async fn finish_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let p: FinishPayload = serde_json::from_value(job.payload)?;
    let row: Option<(Id, String, Option<String>)> = sqlx::query_as(
        "SELECT target_id, object_key, upload_id FROM backups WHERE id = $1 AND status = 'running'",
    )
    .bind(p.backup_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((target, key, Some(upload_id))) = row else {
        return Ok(());
    };
    if !p.ok {
        let error = if p.error.is_empty() {
            "the backup failed"
        } else {
            &p.error
        };
        return fail(&state, p.backup_id, error).await;
    }
    let completed = async {
        let (_, t) = load_target(&state, target)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        t.complete_multipart(&key, &upload_id, &p.etags).await
    }
    .await;
    match completed {
        Ok(()) => {
            sqlx::query(
                "UPDATE backups SET status = 'completed', size_bytes = $2, upload_id = NULL,
                        finished_at = now()
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(p.backup_id)
            .bind(i64::try_from(p.size_bytes).unwrap_or(i64::MAX))
            .execute(&state.db)
            .await?;
            tracing::info!(backup = %p.backup_id, bytes = p.size_bytes, "backup completed");
            Ok(())
        }
        // Retried with backoff; the last attempt gives up on the backup.
        Err(e) if job.attempts < job.max_attempts => Err(e),
        Err(e) => fail(&state, p.backup_id, &format!("{e:#}")).await,
    }
}

/// Restore a completed backup into its service's running database. Returns
/// the restore id; the node reports the outcome.
pub async fn start_restore(state: &AppState, backup: Id, created_by: Id) -> AppResult<Id> {
    let row: Option<(Option<Id>, Id, String, String)> = sqlx::query_as(
        "SELECT service_instance_id, target_id, engine, object_key FROM backups
         WHERE id = $1 AND status = 'completed'",
    )
    .bind(backup)
    .fetch_optional(&state.db)
    .await?;
    let Some((Some(instance), target_id, engine, key)) = row else {
        return Err(AppError::Conflict(
            "only a completed backup can be restored".into(),
        ));
    };
    let engine = Engine::parse(&engine).ok_or_else(|| anyhow::anyhow!("unknown engine"))?;
    if engine == Engine::Redis {
        return Err(AppError::Conflict(
            "restoring Redis backups is not supported yet: download the RDB file and load it manually"
                .into(),
        ));
    }
    let (deployment, node) = live_node(state, instance)
        .await
        .map_err(|e| AppError::Conflict(e.to_string()))?;
    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backups WHERE service_instance_id = $1 AND status = 'running')",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    if running {
        return Err(AppError::Conflict(
            "wait for the running backup to finish".into(),
        ));
    }
    let (workspace, target) = load_target(state, target_id).await?;
    let id = Id::new();
    let inserted: Option<Id> = sqlx::query_scalar(
        "INSERT INTO backup_restores (id, backup_id, service_instance_id, node_id, created_by)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(backup)
    .bind(instance)
    .bind(node)
    .bind(created_by)
    .fetch_optional(&state.db)
    .await?;
    if inserted.is_none() {
        return Err(AppError::Conflict("a restore is already running".into()));
    }
    let request = pb::RestoreRequest {
        restore_id: id.to_string(),
        deployment_id: deployment.to_string(),
        engine: engine.proto() as i32,
        object_url: target.presign_get(&key)?,
        age_identity: (*super::identity(state, workspace).await?).clone(),
    };
    if !state
        .hub
        .send(node, pb::server_message::Msg::Restore(request))
        .await
    {
        sqlx::query(
            "UPDATE backup_restores SET status = 'failed', error = 'the server is offline',
                    finished_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&state.db)
        .await?;
        return Err(AppError::Conflict(
            "the server running the database is offline".into(),
        ));
    }
    Ok(id)
}
