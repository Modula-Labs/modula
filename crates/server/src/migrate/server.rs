//! Migrating a server: a new server at any connected provider, or one that
//! already exists, then every service on the old one moved to it, volume
//! data included, with downtime per stateful service. A step machine over a
//! `server_migrations` row, polled by a self-re-enqueuing job:
//!
//! - create: the API created the new server (the provisioning path,
//!   followed by `provision::track`) before the first poll;
//! - join: until it joined, is online and every provider firewall admits it
//!   (an existing target starts here, already joined);
//! - move: one [`super::moves`] move per instance on the old server (its own
//!   server, or only replicas of it), in turn: services with volumes first;
//! - finish: anything placed on the old server meanwhile gets moved too;
//! - dns: [`super::dns`] points the records at the new server (a main server
//!   comes here from `promote`), with retries; a failure leaves everything
//!   running on the new server and only this step to retry;
//!   then the old server is marked `pending_deletion`. It is never deleted
//!   here: the user confirms that (DELETE /api/servers/{id}).
//!
//! While a migration runs (and after), placement puts nothing new on the old
//! server (deploy/placement.rs). A failure stops with the old server intact;
//! [`abort`] moves everything back.

use std::time::Duration;

use modula_id::Id;
use serde_json::json;
use time::OffsetDateTime;

use super::moves::{self, NewMove, Poll, Table, begin, end};
use crate::error::AppResult;
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

pub const MIGRATE_JOB: &str = "migrate.server";
const POLL: Duration = Duration::from_secs(3);
/// A row that never got its new server (the API died in between).
const CREATE_GRACE: time::Duration = time::Duration::minutes(5);
/// How long the target's address or the firewalls may keep a migration
/// waiting before its first copy.
const ADMIT_WAIT: time::Duration = time::Duration::minutes(5);

pub async fn enqueue(state: &AppState, id: Id, after: Duration) -> AppResult<()> {
    jobs::enqueue(
        &state.db,
        NewJob {
            kind: MIGRATE_JOB,
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

pub async fn migrate_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let id: Id = serde_json::from_value(job.payload["id"].clone())?;
    let done = poll(&state, id)
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
    source_node_id: Option<Id>,
    target_server_id: Option<Id>,
    target_node_id: Option<Id>,
    status: String,
    step: String,
    created_by: Option<Id>,
    created_at: OffsetDateTime,
    main: bool,
}

pub(super) async fn fail(state: &AppState, id: Id, error: &str) -> AppResult<bool> {
    end(&state.db, Table::Migrations, id, "failed", Some(error)).await?;
    sqlx::query(
        "UPDATE server_migrations SET status = 'failed', error = $2, updated_at = now(), finished_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(error)
    .execute(&state.db)
    .await?;
    // A main migration's maintenance ends with it; its bundle goes.
    super::promote::remove_bundle(state, id);
    super::maintenance::sync(state).await?;
    Ok(true)
}

pub(super) async fn set_step(state: &AppState, id: Id, step: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE server_migrations SET step = $2, updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(step)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// One step of migration `id`. Returns true once there is nothing left to do.
pub async fn poll(state: &AppState, id: Id) -> AppResult<bool> {
    let r: Option<Row> = sqlx::query_as(
        "SELECT workspace_id, source_node_id, target_server_id, target_node_id,
                status, step, created_by, created_at, main
         FROM server_migrations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = r else { return Ok(true) };
    if r.status != "running" {
        return Ok(true);
    }
    if r.step == "dns" {
        return super::dns::poll(state, id).await;
    }
    let Some(source) = r.source_node_id else {
        return fail(state, id, "the old server was removed").await;
    };
    match r.step.as_str() {
        "create" => {
            if OffsetDateTime::now_utc() - r.created_at > CREATE_GRACE {
                return fail(state, id, "the new server was not created").await;
            }
            Ok(false)
        }
        "join" => {
            let (node, name) = match joined(state, &r).await? {
                Joined::Waiting => return Ok(false),
                Joined::Failed(why) => return fail(state, id, &why).await,
                Joined::Ready { node, name } => (node, name),
            };
            match admit_target(state, id, &r, source, node).await? {
                Admit::Wait => return Ok(false),
                Admit::Failed(why) => return fail(state, id, &why).await,
                Admit::Ready => {}
            }
            end(&state.db, Table::Migrations, id, "done", None).await?;
            sqlx::query(
                "UPDATE server_migrations SET target_node_id = $2, updated_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(node)
            .execute(&state.db)
            .await?;
            begin(
                &state.db,
                Table::Migrations,
                id,
                &format!("Moving services to {name}"),
            )
            .await?;
            let n = plan(state, id, &r, source, node).await?;
            if n == 0 {
                end(
                    &state.db,
                    Table::Migrations,
                    id,
                    "done",
                    Some("nothing ran there"),
                )
                .await?;
            }
            set_step(state, id, "move").await?;
            Ok(false)
        }
        "move" => {
            let next: Option<(Id, String, String)> = sqlx::query_as(
                "SELECT m.id, m.status, s.name || ' (' || e.name || ')'
                 FROM instance_moves m
                 JOIN service_instances i ON i.id = m.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN environments e ON e.id = i.environment_id
                 WHERE m.migration_id = $1 AND m.status <> 'done'
                 ORDER BY m.created_at, m.id LIMIT 1",
            )
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
            let Some((mv, status, label)) = next else {
                end(&state.db, Table::Migrations, id, "done", None).await?;
                if r.main {
                    // The control plane moves too (it re-plans stragglers).
                    return super::promote::enter(state, id).await;
                }
                set_step(state, id, "finish").await?;
                return Ok(false);
            };
            if status == "pending" {
                let started = sqlx::query(
                    "UPDATE instance_moves SET status = 'running', phase_at = now(), updated_at = now()
                     WHERE id = $1",
                )
                .bind(mv)
                .execute(&state.db)
                .await;
                match started {
                    Err(sqlx::Error::Database(d))
                        if d.constraint() == Some("instance_moves_active_key") =>
                    {
                        let why = format!("{label} is being moved elsewhere at the same time");
                        return fail(state, id, &why).await;
                    }
                    r => {
                        r?;
                    }
                }
            }
            match moves::poll(state, mv).await? {
                Poll::Pending | Poll::Done => Ok(false),
                Poll::Failed(_) | Poll::Stopped => {
                    let error: Option<String> =
                        sqlx::query_scalar("SELECT error FROM instance_moves WHERE id = $1")
                            .bind(mv)
                            .fetch_one(&state.db)
                            .await?;
                    let why = error.unwrap_or_else(|| "it was stopped".into());
                    fail(state, id, &format!("moving {label} failed: {why}")).await
                }
            }
        }
        "promote" => super::promote::poll(state, id).await,
        "finish" => {
            let Some(target) = r.target_node_id else {
                return fail(state, id, "the new server was removed").await;
            };
            // Placed on the old server while this ran (before placement
            // started avoiding it): move those too.
            if plan(state, id, &r, source, target).await? > 0 {
                set_step(state, id, "move").await?;
                return Ok(false);
            }
            begin(&state.db, Table::Migrations, id, super::dns::STEP_LABEL).await?;
            set_step(state, id, "dns").await?;
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Where step `join` stands.
enum Joined {
    Waiting,
    Failed(String),
    Ready { node: Id, name: String },
}

async fn joined(state: &AppState, r: &Row) -> AppResult<Joined> {
    let Some(server) = r.target_server_id else {
        // An existing target: there is nothing to wait for but itself.
        let Some(node) = r.target_node_id else {
            return Ok(Joined::Failed("the new server was deleted".into()));
        };
        let name: Option<String> =
            sqlx::query_scalar("SELECT name FROM nodes WHERE id = $1 AND status <> 'revoked'")
                .bind(node)
                .fetch_optional(&state.db)
                .await?;
        return Ok(match name {
            Some(name) => Joined::Ready { node, name },
            None => Joined::Failed("the target server was removed".into()),
        });
    };
    let s: Option<(String, Option<String>, Option<Id>, String)> = sqlx::query_as(
        "SELECT status, error, node_id, name FROM provisioned_servers WHERE id = $1",
    )
    .bind(server)
    .fetch_optional(&state.db)
    .await?;
    Ok(match s {
        None => Joined::Failed("the new server was deleted".into()),
        Some((status, error, _, _)) if status == "failed" => {
            let why = error.unwrap_or_else(|| "it did not come up".into());
            Joined::Failed(format!("the new server failed: {why}"))
        }
        Some((status, _, Some(node), name)) if status == "ready" => Joined::Ready { node, name },
        Some(_) => Joined::Waiting,
    })
}

enum Admit {
    Ready,
    Wait,
    Failed(String),
}

/// Before the first copy: the target is online with a known address, and the
/// firewalls of the providers involved have been updated with it. The queued
/// sync at its first hello can't be relied on for that (it runs later, and an
/// EC2 target's address only exists once associated), so this one is awaited.
async fn admit_target(
    state: &AppState,
    id: Id,
    r: &Row,
    source: Id,
    target: Id,
) -> AppResult<Admit> {
    // The main server's successor serves every workspace, like the old one.
    if r.main {
        let shared = sqlx::query(
            "UPDATE nodes SET workspace_id = NULL WHERE id = $1 AND workspace_id IS NOT NULL",
        )
        .bind(target)
        .execute(&state.db)
        .await?;
        if shared.rows_affected() > 0 {
            let peers = crate::mesh::bump_peers(&state.db, target).await?;
            state.hub.kick_all(&peers);
        }
    }
    let online = state.hub.is_connected(target)
        || sqlx::query_scalar::<_, bool>("SELECT status = 'online' FROM nodes WHERE id = $1")
            .bind(target)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(false);
    let address: Option<String> = sqlx::query_scalar(
        "SELECT coalesce(n.public_ip, t.ipv4) FROM nodes n
         LEFT JOIN provisioned_servers t ON t.id = $2
         WHERE n.id = $1",
    )
    .bind(target)
    .bind(r.target_server_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let blocked = if !online {
        Some("the target server went offline".to_owned())
    } else if address.is_none() {
        Some("the target server has no public address yet".to_owned())
    } else {
        let workspaces: Vec<Id> = sqlx::query_scalar(
            "SELECT $1::text::id UNION
             SELECT workspace_id FROM provisioned_servers
             WHERE node_id = ANY($2::text[]::id[]) AND status NOT IN ('failed', 'deleting')",
        )
        .bind(r.workspace_id.to_string())
        .bind(vec![source.to_string(), target.to_string()])
        .fetch_all(&state.db)
        .await?;
        match crate::provision::mesh::sync_integrations(state, Some(&workspaces)).await {
            Ok(()) => None,
            Err(e) => Some(format!("{e:#}")),
        }
    };
    let Some(why) = blocked else {
        // The other workspaces' firewalls catch up in the background.
        if let Err(e) = crate::provision::mesh::enqueue_sync(&state.db).await {
            tracing::warn!(%id, error = ?e, "cannot queue the firewall update");
        }
        state.jobs_notify.notify_one();
        return Ok(Admit::Ready);
    };
    let waited: time::Duration = sqlx::query_scalar::<_, OffsetDateTime>(
        "UPDATE server_migrations SET mesh_wait_since = coalesce(mesh_wait_since, now())
         WHERE id = $1 RETURNING mesh_wait_since",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .map(|since| OffsetDateTime::now_utc() - since)?;
    if waited > ADMIT_WAIT {
        return Ok(Admit::Failed(format!(
            "the target server could not be admitted to the private network: {why}"
        )));
    }
    tracing::info!(%id, reason = %why, "waiting to admit the migration's target server");
    Ok(Admit::Wait)
}

/// [`plan`] for a migration by id (the promote step's last check).
pub(super) async fn plan_moves(
    state: &AppState,
    id: Id,
    source: Id,
    target: Id,
) -> AppResult<usize> {
    let r: Row = sqlx::query_as(
        "SELECT workspace_id, source_node_id, target_server_id, target_node_id,
                status, step, created_by, created_at, main
         FROM server_migrations WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    plan(state, id, &r, source, target).await
}

/// Queue a move for every instance still on `source` (its own server, or a
/// replica of it there) that this migration hasn't moved yet. Services with
/// volumes go first, so databases are back before the apps using them.
async fn plan(state: &AppState, id: Id, r: &Row, source: Id, target: Id) -> AppResult<usize> {
    let todo: Vec<(Id, bool)> = sqlx::query_as(
        "SELECT i.id, i.node_id = $1 AS primary_here
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         WHERE (i.node_id = $1
                OR EXISTS (SELECT 1 FROM deployment_replicas rp JOIN deployments d ON d.id = rp.deployment_id
                           WHERE d.service_instance_id = i.id AND rp.node_id = $1
                             AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')))
           AND NOT EXISTS (SELECT 1 FROM instance_moves m WHERE m.migration_id = $2 AND m.service_instance_id = i.id)
         ORDER BY EXISTS (SELECT 1 FROM volumes v WHERE v.service_instance_id = i.id) DESC, s.name, i.id",
    )
    .bind(source)
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let mut tx = state.db.begin().await?;
    for (instance, primary) in &todo {
        moves::create(
            &mut *tx,
            NewMove {
                workspace: r.workspace_id,
                instance: *instance,
                migration: Some(id),
                kind: if *primary { "primary" } else { "replicas" },
                from: Some(source),
                to: target,
                created_by: r.created_by,
                status: "pending",
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(todo.len())
}

/// Try a migration's failed DNS step again. False when it isn't one.
pub async fn retry_dns(state: &AppState, id: Id) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    let claimed = sqlx::query(
        "UPDATE server_migrations SET status = 'running', error = NULL, finished_at = NULL,
                dns_attempts = 0, dns_next_at = NULL, updated_at = now()
         WHERE id = $1 AND status = 'failed' AND step = 'dns'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(false);
    }
    begin(&mut *tx, Table::Migrations, id, super::dns::STEP_LABEL).await?;
    tx.commit().await?;
    enqueue(state, id, Duration::ZERO).await?;
    Ok(true)
}

/// Stop migration `id` and move what it moved back to the old server. The new
/// server is kept (delete it from Servers). False when it can't be aborted
/// (finished, or already aborted).
pub async fn abort(state: &AppState, id: Id) -> AppResult<bool> {
    let claimed = sqlx::query(
        "UPDATE server_migrations SET status = 'aborting', promote_ticket_hash = NULL, updated_at = now()
         WHERE id = $1 AND status IN ('running', 'failed')
           AND NOT (step = 'promote' AND promote_phase = 'switch') AND step <> 'dns'",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(false);
    }
    super::promote::remove_bundle(state, id);
    super::maintenance::sync(state).await?;
    let moved: Vec<(Id, String)> = sqlx::query_as(
        "SELECT id, status FROM instance_moves WHERE migration_id = $1
         ORDER BY created_at DESC, id DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    begin(&state.db, Table::Migrations, id, "Moving services back").await?;
    for (mv, status) in moved {
        if status == "pending" {
            sqlx::query(
                "UPDATE instance_moves SET status = 'aborted', updated_at = now() WHERE id = $1",
            )
            .bind(mv)
            .execute(&state.db)
            .await?;
            continue;
        }
        if let Err(e) = moves::restore(state, mv, true).await {
            let why = moves::error_text(&e);
            end(&state.db, Table::Migrations, id, "failed", Some(&why)).await?;
            sqlx::query(
                "UPDATE server_migrations SET status = 'failed', error = $2, updated_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(&why)
            .execute(&state.db)
            .await?;
            return Err(e);
        }
    }
    end(&state.db, Table::Migrations, id, "done", None).await?;
    sqlx::query(
        "UPDATE server_migrations SET status = 'aborted', updated_at = now(), finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(true)
}
