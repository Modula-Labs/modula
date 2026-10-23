//! Backups: storage targets per workspace, schedules and backups per
//! database service instance, and the control plane's own backups.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::backup::retention::{self, Policy};
use crate::backup::{self, Kind, s3};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::validate;

// ---------------------------------------------------------------- targets

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: Id,
    pub name: String,
    pub provider: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn list_targets(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<Target>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT id, name, provider, endpoint, region, bucket, prefix, access_key_id, created_at
             FROM backup_targets WHERE workspace_id = $1 ORDER BY name",
        )
        .bind(grant.id())
        .fetch_all(&state.db)
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewTarget {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

fn credential(raw: &str, what: &str) -> AppResult<String> {
    let v = raw.trim();
    if v.is_empty() || v.len() > 256 || !v.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::bad_request(format!("invalid {what}")));
    }
    Ok(v.to_owned())
}

/// Add a target. The credentials are checked by writing and deleting a small
/// probe object before anything is stored.
pub async fn create_target(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<NewTarget>,
) -> AppResult<Json<Target>> {
    let workspace = grant.id();
    let name = validate::display_name(&req.name, "name")?;
    let provider = s3::Provider::parse(&req.provider)
        .ok_or_else(|| AppError::bad_request("provider must be s3, r2, b2 or minio"))?;
    let region = s3::region(provider, &req.region)?;
    let target = s3::Target {
        provider,
        endpoint: s3::endpoint(provider, &req.endpoint, &region)?,
        region,
        bucket: s3::bucket(&req.bucket)?,
        prefix: s3::prefix(&req.prefix)?,
        access_key_id: credential(&req.access_key_id, "access key id")?,
        secret: zeroize::Zeroizing::new(credential(&req.secret_access_key, "secret access key")?),
    };
    target
        .probe()
        .await
        .map_err(|e| AppError::bad_request(format!("the bucket is not usable: {e:#}")))?;

    let id = Id::new();
    let row = id.to_string();
    let sealed = state
        .keys
        .seal(
            &state.db,
            target.secret.as_bytes(),
            backup::target_aad(workspace, &row),
        )
        .await?;
    let mut tx = state.db.begin().await?;
    let created: Option<Target> = sqlx::query_as(
        "INSERT INTO backup_targets
           (id, workspace_id, name, provider, endpoint, region, bucket, prefix, access_key_id,
            secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (workspace_id, name) DO NOTHING
         RETURNING id, name, provider, endpoint, region, bucket, prefix, access_key_id, created_at",
    )
    .bind(id)
    .bind(workspace)
    .bind(&name)
    .bind(provider.as_str())
    .bind(target.endpoint.as_str())
    .bind(&target.region)
    .bind(&target.bucket)
    .bind(&target.prefix)
    .bind(&target.access_key_id)
    .bind(&sealed)
    .bind(actor.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let created =
        created.ok_or_else(|| AppError::Conflict(format!("a target named {name} exists")))?;
    Entry::new("backup_target.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("backup_target", id)
        .metadata(json!({ "name": name, "provider": provider.as_str(), "bucket": target.bucket }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(created))
}

/// Delete a target that no schedule uses. Its backup records go with it;
/// the objects stay in the bucket (they remain decryptable with the
/// workspace's key while the workspace exists).
pub async fn delete_target(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let (schedules, self_backup, running): (i64, bool, bool) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM backup_schedules WHERE target_id = $1),
                EXISTS (SELECT 1 FROM instance_backup WHERE target_id = $1),
                EXISTS (SELECT 1 FROM backups WHERE target_id = $1 AND status = 'running')",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if schedules > 0 {
        return Err(AppError::Conflict(format!(
            "{schedules} backup schedule(s) use this target; move them to another target first"
        )));
    }
    if self_backup {
        return Err(AppError::Conflict(
            "the control plane's own backups use this target".into(),
        ));
    }
    if running {
        return Err(AppError::Conflict(
            "a backup to this target is running".into(),
        ));
    }
    let name: String =
        sqlx::query_scalar("DELETE FROM backup_targets WHERE id = $1 RETURNING name")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    Entry::new("backup_target.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup_target", id)
        .metadata(json!({ "name": name }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

// ---------------------------------------------------------------- backups

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    pub id: Id,
    pub status: String,
    pub kind: String,
    pub size_bytes: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub error: Option<String>,
    pub target_name: String,
}

macro_rules! select_backups {
    ($rest:literal) => {
        concat!(
            "SELECT b.id, b.status, b.kind, b.size_bytes, b.started_at, b.finished_at, b.error,
                    t.name AS target_name
             FROM backups b JOIN backup_targets t ON t.id = b.target_id ",
            $rest
        )
    };
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub enabled: bool,
    pub target_id: Id,
    pub hour_utc: i16,
    pub retention: Policy,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Restore {
    pub id: Id,
    pub backup_id: Id,
    pub status: String,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceBackups {
    pub schedule: Option<Schedule>,
    pub backups: Vec<Backup>,
    pub restores: Vec<Restore>,
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<InstanceBackups>> {
    let instance = grant.id();
    let schedule: Option<(bool, Id, i16, i32, i32, i32)> = sqlx::query_as(
        "SELECT enabled, target_id, hour_utc, daily_days, weekly_days, monthly_days
         FROM backup_schedules WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let backups = sqlx::query_as(select_backups!(
        "WHERE b.service_instance_id = $1 ORDER BY b.started_at DESC LIMIT 200"
    ))
    .bind(instance)
    .fetch_all(&state.db)
    .await?;
    let restores = sqlx::query_as(
        "SELECT id, backup_id, status, error, started_at, finished_at FROM backup_restores
         WHERE service_instance_id = $1 ORDER BY started_at DESC LIMIT 10",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(InstanceBackups {
        schedule: schedule.map(|(enabled, target_id, hour_utc, d, w, m)| Schedule {
            enabled,
            target_id,
            hour_utc,
            retention: Policy {
                daily_days: d,
                weekly_days: w,
                monthly_days: m,
            },
        }),
        backups,
        restores,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleBody {
    pub enabled: bool,
    pub target_id: Id,
    pub hour_utc: i16,
    pub retention: Option<Policy>,
}

/// A target id from a request body, checked to belong to `workspace` (a
/// target elsewhere is reported as unknown, like one that does not exist).
async fn own_target(state: &AppState, workspace: Id, target: Id) -> AppResult<()> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backup_targets WHERE id = $1 AND workspace_id = $2)",
    )
    .bind(target)
    .bind(workspace)
    .fetch_one(&state.db)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request("unknown backup target"))
    }
}

fn hour(h: i16) -> AppResult<i16> {
    if (0..=23).contains(&h) {
        Ok(h)
    } else {
        Err(AppError::bad_request("hourUtc must be 0–23"))
    }
}

pub async fn put_schedule(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<ScheduleBody>,
) -> AppResult<Json<InstanceBackups>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    backup::run::instance_engine(&state, instance).await?;
    own_target(&state, workspace, req.target_id).await?;
    let hour_utc = hour(req.hour_utc)?;
    let retention = req.retention.unwrap_or(retention::DEFAULT);
    for days in [
        retention.daily_days,
        retention.weekly_days,
        retention.monthly_days,
    ] {
        if !(0..=3660).contains(&days) {
            return Err(AppError::bad_request("retention days must be 0–3660"));
        }
    }
    let mut tx = state.db.begin().await?;
    // A new schedule's first backup is at its next slot, not right away.
    sqlx::query(
        "INSERT INTO backup_schedules
           (service_instance_id, enabled, target_id, hour_utc, daily_days, weekly_days,
            monthly_days, last_scheduled_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         ON CONFLICT (service_instance_id) DO UPDATE
         SET enabled = $2, target_id = $3, hour_utc = $4, daily_days = $5, weekly_days = $6,
             monthly_days = $7, updated_at = now()",
    )
    .bind(instance)
    .bind(req.enabled)
    .bind(req.target_id)
    .bind(hour_utc)
    .bind(retention.daily_days)
    .bind(retention.weekly_days)
    .bind(retention.monthly_days)
    .execute(&mut *tx)
    .await?;
    Entry::new("backup.schedule.update", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({
            "enabled": req.enabled,
            "target": req.target_id,
            "hourUtc": hour_utc,
            "retention": retention,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    list(State(state), grant).await
}

async fn load(state: &AppState, id: Id) -> AppResult<Backup> {
    Ok(sqlx::query_as(select_backups!("WHERE b.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?)
}

/// Back up now, to the schedule's target.
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Backup>> {
    let instance = grant.id();
    let target: Option<Id> =
        sqlx::query_scalar("SELECT target_id FROM backup_schedules WHERE service_instance_id = $1")
            .bind(instance)
            .fetch_optional(&state.db)
            .await?;
    let target = target.ok_or_else(|| {
        AppError::Conflict("choose a backup target first (in the backup schedule)".into())
    })?;
    let id = backup::run::start(
        &state,
        Some(instance),
        target,
        Kind::Manual,
        Some(actor.user_id),
    )
    .await?;
    Entry::new("backup.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup", id)
        .metadata(json!({ "serviceInstance": instance, "target": target }))
        .record(&state.db)
        .await?;
    load(&state, id).await.map(Json)
}

pub async fn restore(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Restore>> {
    let backup = grant.id();
    let id = backup::run::start_restore(&state, backup, actor.user_id).await?;
    Entry::new("backup.restore", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup", backup)
        .metadata(json!({ "restore": id }))
        .record(&state.db)
        .await?;
    Ok(Json(
        sqlx::query_as(
            "SELECT id, backup_id, status, error, started_at, finished_at
             FROM backup_restores WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await?,
    ))
}

/// Delete a backup's object from its target and mark it deleted.
pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Backup>> {
    let id = grant.id();
    let (status, target, key): (String, Id, String) =
        sqlx::query_as("SELECT status, target_id, object_key FROM backups WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    match status.as_str() {
        "running" => return Err(AppError::Conflict("this backup is still running".into())),
        "deleted" => return Err(AppError::Conflict("this backup was already deleted".into())),
        // Failed uploads were aborted: there is no object.
        "failed" => {}
        _ => {
            let (_, t) = backup::load_target(&state, target).await?;
            t.delete(&key)
                .await
                .map_err(|e| AppError::Unavailable(format!("cannot delete the backup: {e:#}")))?;
        }
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE backups SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("backup.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    load(&state, id).await.map(Json)
}

// ---------------------------------------------------------------- control plane

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfBackup {
    pub enabled: bool,
    pub target_id: Option<Id>,
    pub hour_utc: i16,
    pub backups: Vec<Backup>,
}

pub async fn get_self_backup(State(state): State<AppState>) -> AppResult<Json<SelfBackup>> {
    let (enabled, target_id, hour_utc): (bool, Option<Id>, i16) =
        sqlx::query_as("SELECT enabled, target_id, hour_utc FROM instance_backup")
            .fetch_one(&state.db)
            .await?;
    let backups = sqlx::query_as(select_backups!(
        "WHERE b.service_instance_id IS NULL ORDER BY b.started_at DESC LIMIT 50"
    ))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(SelfBackup {
        enabled,
        target_id,
        hour_utc,
        backups,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelfBackupBody {
    pub enabled: bool,
    pub target_id: Option<Id>,
    pub hour_utc: i16,
}

/// Instance admins pick a target from a workspace they administer: the
/// control plane's dumps are encrypted to a key only the master key derives,
/// so the workspace's members can see the objects but not read them.
pub async fn put_self_backup(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<SelfBackupBody>,
) -> AppResult<Json<SelfBackup>> {
    let hour_utc = hour(req.hour_utc)?;
    if req.enabled && req.target_id.is_none() {
        return Err(AppError::bad_request("choose a backup target"));
    }
    if let Some(target) = req.target_id {
        let admin: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM backup_targets t
                            JOIN workspace_members m ON m.workspace_id = t.workspace_id
                            WHERE t.id = $1 AND m.user_id = $2 AND m.role = 'admin')",
        )
        .bind(target)
        .bind(actor.user_id)
        .fetch_one(&state.db)
        .await?;
        if !admin {
            return Err(AppError::bad_request("unknown backup target"));
        }
    }
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE instance_backup SET enabled = $1, target_id = $2, hour_utc = $3, updated_by = $4,
                updated_at = now(),
                last_scheduled_at = coalesce(last_scheduled_at, now())",
    )
    .bind(req.enabled)
    .bind(req.target_id)
    .bind(hour_utc)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("instance.self_backup.update", &meta)
        .actor(&actor)
        .metadata(json!({ "enabled": req.enabled, "target": req.target_id, "hourUtc": hour_utc }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    get_self_backup(State(state)).await
}
