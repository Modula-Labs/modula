//! Backups: storage targets per workspace, schedules and backups per
//! database service instance, and the control plane's own backups.
//!
//! A schedule (or "Back up now") points at an S3 target, a bucket service of
//! the workspace or the disk of the server running the database; the last
//! two become `bucket` and `local` targets on first use (see
//! [`crate::backup::store`]).

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
    /// `s3`, `bucket` (a bucket service) or `local` (a server's disk).
    pub kind: String,
    pub provider: Option<String>,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    /// Bucket targets: the bucket service's instance and names.
    pub bucket_instance_id: Option<Id>,
    pub service_name: Option<String>,
    pub environment_name: Option<String>,
    pub project_name: Option<String>,
    /// Local targets: the server.
    pub node_id: Option<Id>,
    pub node_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

macro_rules! select_targets {
    ($rest:literal) => {
        concat!(
            "SELECT t.id, t.name, t.kind, t.provider, t.endpoint, t.region, t.bucket, t.prefix,
                    t.access_key_id, t.bucket_instance_id, s.name AS service_name,
                    e.name AS environment_name, p.name AS project_name, t.node_id,
                    n.name AS node_name, t.created_at
             FROM backup_targets t
             LEFT JOIN service_instances i ON i.id = t.bucket_instance_id
             LEFT JOIN services s ON s.id = i.service_id
             LEFT JOIN environments e ON e.id = i.environment_id
             LEFT JOIN projects p ON p.id = s.project_id
             LEFT JOIN nodes n ON n.id = t.node_id ",
            $rest
        )
    };
}

async fn targets(state: &AppState, workspace: Id) -> AppResult<Vec<Target>> {
    Ok(
        sqlx::query_as(select_targets!("WHERE t.workspace_id = $1 ORDER BY t.name"))
            .bind(workspace)
            .fetch_all(&state.db)
            .await?,
    )
}

async fn load_target(state: &AppState, id: Id) -> AppResult<Target> {
    Ok(sqlx::query_as(select_targets!("WHERE t.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?)
}

pub async fn list_targets(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<Target>>> {
    targets(&state, grant.id()).await.map(Json)
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
        .probe(s3::http()?, &Id::new().to_string())
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
    let created: Option<Id> = sqlx::query_scalar(
        "INSERT INTO backup_targets
           (id, workspace_id, name, provider, endpoint, region, bucket, prefix, access_key_id,
            secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (workspace_id, name) DO NOTHING
         RETURNING id",
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
    if created.is_none() {
        return Err(AppError::Conflict(format!("a target named {name} exists")));
    }
    Entry::new("backup_target.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("backup_target", id)
        .metadata(json!({ "name": name, "provider": provider.as_str(), "bucket": target.bucket }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    load_target(&state, id).await.map(Json)
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
                EXISTS (SELECT 1 FROM backups WHERE target_id = $1
                        AND status IN ('running', 'deleting'))",
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

// ---------------------------------------------------------------- destinations

/// Where a schedule or "Back up now" sends backups: exactly one of an
/// existing target, a bucket service instance, or the disk of the server
/// running the database.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Destination {
    #[serde(default)]
    pub target_id: Option<Id>,
    #[serde(default)]
    pub bucket_instance_id: Option<Id>,
    #[serde(default)]
    pub local_disk: bool,
}

impl Destination {
    pub(crate) fn given(&self) -> usize {
        usize::from(self.target_id.is_some())
            + usize::from(self.bucket_instance_id.is_some())
            + usize::from(self.local_disk)
    }
}

/// A bucket service a database can back up to.
#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct BucketOption {
    pub instance_id: Id,
    /// `r2` or `bucket` (self-hosted).
    pub template: String,
    pub service_name: String,
    pub environment_name: String,
    pub project_name: String,
    /// Its target, once something backed up to it.
    pub target_id: Option<Id>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ServerOption {
    pub node_id: Id,
    pub name: String,
    pub target_id: Option<Id>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Destinations {
    /// Every target of the workspace (S3, and bucket and disk ones in use).
    pub targets: Vec<Target>,
    pub buckets: Vec<BucketOption>,
    /// The server running the database (its disk), when it has one.
    pub servers: Vec<ServerOption>,
}

/// The Storage choices of a database's Backups tab.
pub async fn destinations(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Destinations>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let buckets = sqlx::query_as(
        "SELECT i.id AS instance_id, s.template, s.name AS service_name,
                e.name AS environment_name, p.name AS project_name, t.id AS target_id
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         JOIN projects p ON p.id = s.project_id
         LEFT JOIN backup_targets t ON t.bucket_instance_id = i.id
         WHERE p.workspace_id = $1 AND s.template IN ('r2', 'bucket')
           AND (s.template <> 'r2' OR EXISTS (SELECT 1 FROM r2_buckets r WHERE r.service_instance_id = i.id))
         ORDER BY p.name, s.name, e.name",
    )
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;
    let servers = sqlx::query_as(
        "SELECT n.id AS node_id, n.name, t.id AS target_id
         FROM service_instances i JOIN nodes n ON n.id = i.node_id
         LEFT JOIN backup_targets t ON t.workspace_id = $2 AND t.node_id = n.id
         WHERE i.id = $1 AND n.status <> 'revoked'",
    )
    .bind(instance)
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Destinations {
        targets: targets(&state, workspace).await?,
        buckets,
        servers,
    }))
}

/// Insert an implicit target named `name` (or `name · <id>` if taken).
#[allow(clippy::too_many_arguments)]
async fn insert_implicit(
    tx: &mut sqlx::PgConnection,
    workspace: Id,
    name: &str,
    kind: &str,
    provider: Option<&str>,
    prefix: &str,
    bucket_instance: Option<Id>,
    node: Option<Id>,
    actor: Id,
) -> AppResult<(Id, String)> {
    let id = Id::new();
    for name in [name.to_owned(), format!("{name} · {id}")] {
        let created: Option<Id> = sqlx::query_scalar(
            "INSERT INTO backup_targets
               (id, workspace_id, name, kind, provider, prefix, bucket_instance_id, node_id, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (workspace_id, name) DO NOTHING RETURNING id",
        )
        .bind(id)
        .bind(workspace)
        .bind(&name)
        .bind(kind)
        .bind(provider)
        .bind(prefix)
        .bind(bucket_instance)
        .bind(node)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        if created.is_some() {
            return Ok((id, name));
        }
    }
    Err(AppError::Conflict(
        "could not name the backup target".into(),
    ))
}

/// The target a destination stands for, made on first use for bucket
/// services and disks. `instance` is the database being backed up.
pub(crate) async fn resolve_destination(
    state: &AppState,
    actor: &Actor,
    meta: &RequestMeta,
    workspace: Id,
    instance: Id,
    dest: &Destination,
) -> AppResult<Id> {
    if dest.given() != 1 {
        return Err(AppError::bad_request(
            "give one of targetId, bucketInstanceId or localDisk",
        ));
    }
    if let Some(target) = dest.target_id {
        own_target(state, workspace, target).await?;
        return Ok(target);
    }
    let mut tx = state.db.begin().await?;
    let made = if let Some(bucket) = dest.bucket_instance_id {
        // A bucket elsewhere is reported as unknown, like one that does not exist.
        let row: Option<(String, String, String, Option<Id>)> = sqlx::query_as(
            "SELECT s.template, s.name, e.name,
                    (SELECT t.id FROM backup_targets t WHERE t.bucket_instance_id = i.id)
             FROM service_instances i
             JOIN services s ON s.id = i.service_id
             JOIN environments e ON e.id = i.environment_id
             JOIN projects p ON p.id = s.project_id
             WHERE i.id = $1 AND p.workspace_id = $2 AND s.template IN ('r2', 'bucket')",
        )
        .bind(bucket)
        .bind(workspace)
        .fetch_optional(&mut *tx)
        .await?;
        let (template, service, env, existing) =
            row.ok_or_else(|| AppError::bad_request("unknown bucket service"))?;
        if let Some(t) = existing {
            return Ok(t);
        }
        let provider = if template == "r2" { "r2" } else { "minio" };
        let (id, name) = insert_implicit(
            &mut tx,
            workspace,
            &format!("{service} ({env})"),
            "bucket",
            Some(provider),
            "modula-backups/",
            Some(bucket),
            None,
            actor.user_id,
        )
        .await?;
        (
            id,
            name,
            json!({ "kind": "bucket", "bucketInstance": bucket }),
        )
    } else {
        let node: Option<(Id, String, Option<Id>)> = sqlx::query_as(
            "SELECT n.id, n.name,
                    (SELECT t.id FROM backup_targets t WHERE t.workspace_id = $2 AND t.node_id = n.id)
             FROM service_instances i JOIN nodes n ON n.id = i.node_id
             WHERE i.id = $1 AND n.status <> 'revoked'",
        )
        .bind(instance)
        .bind(workspace)
        .fetch_optional(&mut *tx)
        .await?;
        let (node, node_name, existing) = node.ok_or_else(|| {
            AppError::Conflict("the database runs on no server yet: deploy it first".into())
        })?;
        if let Some(t) = existing {
            return Ok(t);
        }
        let (id, name) = insert_implicit(
            &mut tx,
            workspace,
            &format!("Disk of {node_name}"),
            "local",
            None,
            "",
            None,
            Some(node),
            actor.user_id,
        )
        .await?;
        (id, name, json!({ "kind": "local", "node": node }))
    };
    let (id, name, mut metadata) = made;
    metadata["name"] = json!(name);
    Entry::new("backup_target.create", meta)
        .actor(actor)
        .workspace(workspace)
        .target("backup_target", id)
        .metadata(metadata)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
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
    pub target_kind: String,
    /// `dump`, or `walg` (a base backup for point-in-time recovery).
    pub method: String,
    /// WAL-G's name of a base backup.
    pub walg_name: Option<String>,
}

macro_rules! select_backups {
    ($rest:literal) => {
        concat!(
            "SELECT b.id, b.status, b.kind, b.size_bytes, b.started_at, b.finished_at, b.error,
                    t.name AS target_name, t.kind AS target_kind, b.method, b.walg_name
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
    pub target_name: String,
    pub target_kind: String,
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
    #[allow(clippy::type_complexity)]
    let schedule: Option<(bool, Id, String, String, i16, i32, i32, i32)> = sqlx::query_as(
        "SELECT s.enabled, s.target_id, t.name, t.kind, s.hour_utc, s.daily_days, s.weekly_days,
                s.monthly_days
         FROM backup_schedules s JOIN backup_targets t ON t.id = s.target_id
         WHERE s.service_instance_id = $1",
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
        schedule: schedule.map(
            |(enabled, target_id, target_name, target_kind, hour_utc, d, w, m)| Schedule {
                enabled,
                target_id,
                target_name,
                target_kind,
                hour_utc,
                retention: Policy {
                    daily_days: d,
                    weekly_days: w,
                    monthly_days: m,
                },
            },
        ),
        backups,
        restores,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleBody {
    pub enabled: bool,
    /// Where backups go (see [`Destination`]).
    #[serde(default)]
    pub target_id: Option<Id>,
    #[serde(default)]
    pub bucket_instance_id: Option<Id>,
    #[serde(default)]
    pub local_disk: bool,
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
    let dest = Destination {
        target_id: req.target_id,
        bucket_instance_id: req.bucket_instance_id,
        local_disk: req.local_disk,
    };
    let target = resolve_destination(&state, &actor, &meta, workspace, instance, &dest).await?;
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
    .bind(target)
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
            "target": target,
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

/// Back up now: to the given destination, else the schedule's target.
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    body: Option<Json<Destination>>,
) -> AppResult<Json<Backup>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let dest = body.map(|Json(d)| d).unwrap_or_default();
    let target = if dest.given() > 0 {
        backup::run::instance_engine(&state, instance).await?;
        resolve_destination(&state, &actor, &meta, workspace, instance, &dest).await?
    } else {
        let target: Option<Id> = sqlx::query_scalar(
            "SELECT target_id FROM backup_schedules WHERE service_instance_id = $1",
        )
        .bind(instance)
        .fetch_optional(&state.db)
        .await?;
        target.ok_or_else(|| {
            AppError::Conflict("choose where backups go first (in the backup schedule)".into())
        })?
    };
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
        .workspace(workspace)
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

/// Delete a backup's object and mark it deleted. Objects in a self-hosted
/// bucket or on a server's disk are deleted by that server: the backup is
/// `deleting` until it reports back.
pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Backup>> {
    let id = grant.id();
    let status: String = sqlx::query_scalar("SELECT status FROM backups WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    match status.as_str() {
        "running" => return Err(AppError::Conflict("this backup is still running".into())),
        "deleting" | "deleted" => {
            return Err(AppError::Conflict("this backup was already deleted".into()));
        }
        // Failed uploads were aborted: there is no object.
        "failed" => {
            sqlx::query("UPDATE backups SET status = 'deleted', deleted_at = now() WHERE id = $1")
                .bind(id)
                .execute(&state.db)
                .await?;
        }
        _ => {
            backup::run::remove(&state, id).await?;
        }
    }
    Entry::new("backup.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup", id)
        .record(&state.db)
        .await?;
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

/// Instance admins pick an S3 target from a workspace they administer: the
/// control plane's dumps are encrypted to a key only the master key derives,
/// so the workspace's members can see the objects but not read them. Bucket
/// services and server disks are not offered: the control plane must reach
/// its backups without any of its servers.
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
                            WHERE t.id = $1 AND m.user_id = $2 AND m.role = 'admin'
                              AND t.kind = 's3')",
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
