//! Point-in-time recovery of a Postgres service (see [`crate::backup::pitr`]):
//! its settings and recoverable window, base backups, and restores into a
//! new service or over its own data.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::api::backups::{Backup, Destination, resolve_destination};
use crate::api::services::{CreateService, create_service, validate_service_name};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::backup::pitr::{self, Window};
use crate::backup::{Kind, Store, load_target};
use crate::deploy::{self, env};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    pub target_id: Id,
    pub target_name: String,
    pub target_kind: String,
    pub window_days: i32,
    pub hour_utc: i16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Archiving {
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_archived_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_failed_at: Option<OffsetDateTime>,
    pub failed_count: i64,
    pub in_recovery: bool,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PitrRestore {
    pub id: Id,
    pub source_instance_id: Id,
    pub instance_id: Id,
    pub service_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub target_time: OffsetDateTime,
    pub status: String,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PitrInfo {
    pub config: Option<Config>,
    /// What a restore can reach (None until a base backup exists).
    pub window: Option<Window>,
    /// WAL archiving as the running database reports it.
    pub archiving: Option<Archiving>,
    pub base_backups: Vec<Backup>,
    /// Restores of this database (into itself or new services) and into it.
    pub restores: Vec<PitrRestore>,
}

/// 409 unless the instance's service is a Postgres template service.
async fn postgres(state: &AppState, instance: Id) -> AppResult<()> {
    let (engine, _) = crate::backup::run::instance_engine(state, instance).await?;
    if engine != crate::backup::Engine::Postgres {
        return Err(AppError::Conflict(
            "point-in-time recovery is for Postgres services".into(),
        ));
    }
    Ok(())
}

fn unix(v: &Value) -> Option<OffsetDateTime> {
    v.as_f64()
        .and_then(|t| OffsetDateTime::from_unix_timestamp(t as i64).ok())
}

async fn info(state: &AppState, instance: Id) -> AppResult<PitrInfo> {
    pitr::check_restores(state, Some(instance))
        .await
        .map_err(AppError::Internal)?;
    let config: Option<(bool, Id, String, String, i32, i16)> = sqlx::query_as(
        "SELECT c.enabled, c.target_id, t.name, t.kind, c.window_days, c.hour_utc
         FROM pitr_configs c JOIN backup_targets t ON t.id = c.target_id
         WHERE c.service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let config = config.map(
        |(enabled, target_id, target_name, target_kind, window_days, hour_utc)| Config {
            enabled,
            target_id,
            target_name,
            target_kind,
            window_days,
            hour_utc,
        },
    );
    let status = match &config {
        Some(c) if c.enabled => pitr::wal_status(state, instance).await,
        _ => None,
    };
    let window = match &config {
        Some(c) => pitr::window(state, instance, c.target_id, status.as_ref()).await?,
        None => None,
    };
    let archiving = status.as_ref().map(|s| Archiving {
        last_archived_at: unix(&s["lastArchivedAt"]),
        last_failed_at: unix(&s["lastFailedAt"]),
        failed_count: s["failedCount"].as_i64().unwrap_or(0),
        in_recovery: s["inRecovery"] == true,
    });
    let base_backups = sqlx::query_as(
        "SELECT b.id, b.status, b.kind, b.size_bytes, b.started_at, b.finished_at, b.error,
                t.name AS target_name, t.kind AS target_kind, b.method, b.walg_name
         FROM backups b JOIN backup_targets t ON t.id = b.target_id
         WHERE b.service_instance_id = $1 AND b.method = 'walg'
         ORDER BY b.started_at DESC LIMIT 100",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await?;
    let restores = sqlx::query_as(
        "SELECT r.id, r.source_instance_id, r.instance_id, s.name AS service_name, r.target_time,
                r.status, r.error, r.started_at, r.finished_at
         FROM pitr_restores r
         JOIN service_instances i ON i.id = r.instance_id JOIN services s ON s.id = i.service_id
         WHERE r.source_instance_id = $1 OR r.instance_id = $1
         ORDER BY r.started_at DESC LIMIT 10",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await?;
    Ok(PitrInfo {
        config,
        window,
        archiving,
        base_backups,
        restores,
    })
}

pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<PitrInfo>> {
    info(&state, grant.id()).await.map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PitrBody {
    pub enabled: bool,
    /// Where WAL goes (as for backup schedules); kept when none is given.
    #[serde(default)]
    pub target_id: Option<Id>,
    #[serde(default)]
    pub bucket_instance_id: Option<Id>,
    #[serde(default)]
    pub local_disk: bool,
    #[serde(default)]
    pub window_days: Option<i32>,
    #[serde(default)]
    pub hour_utc: Option<i16>,
}

/// Turn PITR on or off, or change its storage or window. Turning it on or
/// off, or moving it, redeploys a running database (archiving is set when
/// its container starts).
pub async fn put(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<PitrBody>,
) -> AppResult<Json<PitrInfo>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    postgres(&state, instance).await?;
    let window_days = req.window_days.unwrap_or(7);
    if !(1..=35).contains(&window_days) {
        return Err(AppError::bad_request("windowDays must be 1–35"));
    }
    let hour_utc = req.hour_utc.unwrap_or(3);
    if !(0..=23).contains(&hour_utc) {
        return Err(AppError::bad_request("hourUtc must be 0–23"));
    }
    let current: Option<(bool, Id)> = sqlx::query_as(
        "SELECT enabled, target_id FROM pitr_configs WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let dest = Destination {
        target_id: req.target_id,
        bucket_instance_id: req.bucket_instance_id,
        local_disk: req.local_disk,
    };
    let target = match (dest.given(), current) {
        (0, Some((_, t))) => t,
        (0, None) => return Err(AppError::bad_request("choose where WAL goes")),
        _ => resolve_destination(&state, &actor, &meta, workspace, instance, &dest).await?,
    };
    // The database must reach the storage itself.
    let (environment, node): (Id, Option<Id>) =
        sqlx::query_as("SELECT environment_id, node_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let (_, store) = load_target(&state, target).await?;
    if matches!(store, Store::Disk { .. }) && node.is_none() {
        return Err(AppError::Conflict(
            "deploy the database first: WAL goes to the disk of the server it runs on".into(),
        ));
    }
    pitr::storage(store, workspace, instance, environment, node)?;
    let key = pitr::seal_key(&state, workspace, instance, &pitr::generate_key()?).await?;
    let mut tx = state.db.begin().await?;
    // The key is made once: it decrypts everything archived so far.
    sqlx::query(
        "INSERT INTO pitr_configs (service_instance_id, enabled, target_id, window_days, hour_utc,
                                   key_sealed, last_scheduled_at)
         VALUES ($1, $2, $3, $4, $5, $6, now())
         ON CONFLICT (service_instance_id) DO UPDATE
         SET enabled = $2, target_id = $3, window_days = $4, hour_utc = $5, updated_at = now()",
    )
    .bind(instance)
    .bind(req.enabled)
    .bind(target)
    .bind(window_days)
    .bind(hour_utc)
    .bind(&key)
    .execute(&mut *tx)
    .await?;
    Entry::new("pitr.update", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({
            "enabled": req.enabled,
            "target": target,
            "windowDays": window_days,
            "hourUtc": hour_utc,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let changed = current.is_none_or(|(on, t)| on != req.enabled || (req.enabled && t != target));
    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deployments WHERE service_instance_id = $1 AND status = 'active')",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    if changed && running && (req.enabled || current.is_some()) {
        let message = if req.enabled {
            "Point-in-time recovery turned on"
        } else {
            "Point-in-time recovery turned off"
        };
        crate::api::deployments::deploy_instance(
            &state,
            &actor,
            workspace,
            &meta,
            instance,
            Some(message),
        )
        .await?;
    }
    info(&state, instance).await.map(Json)
}

/// Take a base backup now.
pub async fn base_backup(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<PitrInfo>> {
    let instance = grant.id();
    let id = pitr::start_base(&state, instance, Kind::Manual, Some(actor.user_id)).await?;
    Entry::new("pitr.base_backup", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("backup", id)
        .metadata(json!({ "serviceInstance": instance }))
        .record(&state.db)
        .await?;
    info(&state, instance).await.map(Json)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestoreInto {
    /// A new service in the same project (the default).
    New,
    /// This service, replacing its data.
    Same,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreBody {
    #[serde(with = "time::serde::rfc3339")]
    pub target_time: OffsetDateTime,
    pub into: Option<RestoreInto>,
    /// The new service's name (default `<name>-restored`).
    #[serde(default)]
    pub name: Option<String>,
    /// `into: same`: the service's name, typed to confirm.
    #[serde(default)]
    pub confirm: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Restored {
    pub restore_id: Id,
    /// The instance being restored (the new service's, or this one).
    pub instance_id: Id,
    pub service_id: Id,
    pub service_name: String,
}

/// The restore's plan: a base backup at or before the time, within the window.
async fn plan(state: &AppState, instance: Id, at: OffsetDateTime) -> AppResult<(Id, String)> {
    let target: Id =
        sqlx::query_scalar("SELECT target_id FROM pitr_configs WHERE service_instance_id = $1")
            .bind(instance)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::Conflict("point-in-time recovery was never on here".into()))?;
    let status = pitr::wal_status(state, instance).await;
    let window = pitr::window(state, instance, target, status.as_ref())
        .await?
        .ok_or_else(|| AppError::Conflict("there is no base backup yet".into()))?;
    if at < window.from || at > window.to {
        return Err(AppError::bad_request(format!(
            "choose a time between {} and {}",
            pitr::recovery_time(window.from),
            pitr::recovery_time(window.to)
        )));
    }
    let base = pitr::base_for(state, instance, target, at)
        .await?
        .ok_or_else(|| AppError::Conflict("no base backup covers that time".into()))?;
    Ok((target, base))
}

pub async fn restore(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<RestoreBody>,
) -> AppResult<Json<Restored>> {
    let source = grant.id();
    let workspace = grant.workspace_id()?;
    postgres(&state, source).await?;
    let (target, base) = plan(&state, source, req.target_time).await?;
    #[allow(clippy::type_complexity)]
    let (service_id, service_name, project, environment, node): (
        Id,
        String,
        Id,
        Id,
        Option<Id>,
    ) = sqlx::query_as(
        "SELECT s.id, s.name, s.project_id, i.environment_id, i.node_id
             FROM service_instances i JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(source)
    .fetch_one(&state.db)
    .await?;
    let on_disk = matches!(load_target(&state, target).await?.1, Store::Disk { .. });
    let into = req.into.unwrap_or(RestoreInto::New);
    let restore = Id::new();
    let (instance, restored_service, restored_name) = match into {
        RestoreInto::New => {
            let name = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                Some(n) => n.to_owned(),
                None => format!("{}-restored", truncate(&service_name, 54)),
            };
            validate_service_name(&name)?;
            // The data comes with the source's roles: keep its credentials.
            let raw = env::raw_variables(&state.keys, &state.db, environment, source).await?;
            let keep: Vec<(String, String)> = [
                "POSTGRES_USER",
                "POSTGRES_PASSWORD",
                "POSTGRES_DB",
                "REPLICATION_PASSWORD",
            ]
            .iter()
            .filter_map(|k| raw.get(*k).map(|v| ((*k).to_owned(), v.value.clone())))
            .collect();
            let created = create_service(
                &state,
                &actor,
                &meta,
                workspace,
                project,
                CreateService {
                    name,
                    config: None,
                    template: Some("postgres".into()),
                },
                &HashMap::from([(environment, keep)]),
            )
            .await?;
            let instance = created
                .instances
                .iter()
                .find(|i| i.environment_id == environment)
                .map(|i| i.id)
                .ok_or(AppError::NotFound)?;
            // Disk storage is read where it is.
            if on_disk {
                sqlx::query("UPDATE service_instances SET node_id = $2 WHERE id = $1")
                    .bind(instance)
                    .bind(node)
                    .execute(&state.db)
                    .await?;
            }
            sqlx::query(
                "INSERT INTO pitr_restores (id, source_instance_id, instance_id, target_id,
                                            base_backup, target_time, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(restore)
            .bind(source)
            .bind(instance)
            .bind(target)
            .bind(&base)
            .bind(req.target_time)
            .bind(actor.user_id)
            .execute(&state.db)
            .await?;
            (instance, created.id, created.name)
        }
        RestoreInto::Same => {
            if req.confirm.as_deref() != Some(service_name.as_str()) {
                return Err(AppError::bad_request(format!(
                    "type the service's name ({service_name}) to replace its data"
                )));
            }
            let mut tx = state.db.begin().await?;
            // A fresh volume: the old one's data is deleted once its
            // container is gone.
            let old: Option<(Id, String)> = sqlx::query_as(
                "DELETE FROM volumes WHERE service_instance_id = $1 RETURNING id, mount_path",
            )
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?;
            let (old, mount) = old.ok_or_else(|| {
                AppError::Conflict("the database has no volume to restore into".into())
            })?;
            if let Some(n) = node {
                sqlx::query(
                    "INSERT INTO volume_removals (volume_id, node_id) VALUES ($1, $2)
                     ON CONFLICT DO NOTHING",
                )
                .bind(old)
                .bind(n)
                .execute(&mut *tx)
                .await?;
                deploy::bump(&mut *tx, n).await?;
            }
            sqlx::query(
                "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, $3)",
            )
            .bind(Id::new())
            .bind(source)
            .bind(&mount)
            .execute(&mut *tx)
            .await?;
            let inserted = sqlx::query(
                "INSERT INTO pitr_restores (id, source_instance_id, instance_id, target_id,
                                            base_backup, target_time, created_by)
                 VALUES ($1, $2, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            )
            .bind(restore)
            .bind(source)
            .bind(target)
            .bind(&base)
            .bind(req.target_time)
            .bind(actor.user_id)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                return Err(AppError::Conflict("a restore is already running".into()));
            }
            tx.commit().await?;
            if let Some(n) = node {
                state.hub.kick(n);
            }
            (source, service_id, service_name.clone())
        }
    };
    Entry::new("pitr.restore", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", source)
        .metadata(json!({
            "restore": restore,
            "into": instance,
            "targetTime": pitr::recovery_time(req.target_time),
            "baseBackup": base,
        }))
        .record(&state.db)
        .await?;
    crate::api::deployments::deploy_instance(
        &state,
        &actor,
        workspace,
        &meta,
        instance,
        Some(&format!(
            "Restoring {service_name} to {}",
            pitr::recovery_time(req.target_time)
        )),
    )
    .await?;
    Ok(Json(Restored {
        restore_id: restore,
        instance_id: instance,
        service_id: restored_service,
        service_name: restored_name,
    }))
}

fn truncate(s: &str, n: usize) -> &str {
    let s = &s[..s.len().min(n)];
    s.trim_end_matches('-')
}
