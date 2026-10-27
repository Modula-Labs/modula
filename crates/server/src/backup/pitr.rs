//! Point-in-time recovery (PITR) of Postgres services with WAL-G.
//!
//! With PITR on (`pitr_configs`), the database's deployments carry WAL-G's
//! settings in their environment ([`workload`]): the template's start script
//! then archives every WAL segment (`archive_command = wal-g wal-push %p`,
//! at most a minute behind with `archive_timeout = 60`) to the instance's
//! WAL-G storage in its target, encrypted with a per-instance libsodium key.
//! A daily base backup (`wal-g backup-push`, run by the node) anchors replay;
//! base backups older than the window are pruned with their WAL once a newer
//! one covers it. Settings reach a container only when it is created, so
//! turning PITR on or off redeploys the database.
//!
//! The storage is where the database container reaches it: S3-compatible
//! storage over the internet, a self-hosted bucket of the same environment
//! at `<bucket>.modula.internal` (other environments are firewalled off), or
//! the database server's disk, bind-mounted by the node.
//!
//! A restore picks the newest base backup finished before the target time
//! and starts a deployment whose data directory is empty: a new service
//! (default) or the same one on a fresh volume. Its start script fetches the
//! base backup, replays WAL up to the target time and promotes; the restore
//! is done once the database is out of recovery ([`check_restores`]).

use std::collections::HashMap;
use std::time::Duration;

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use zeroize::Zeroizing;

use super::{Kind, Store, load_target};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Where disk storage is mounted in the container (the node's
/// `backup::walg::CONTAINER_DIR`).
pub const CONTAINER_DIR: &str = "/var/lib/modula-wal-g";
/// A restore that hasn't left recovery after this long is given up on.
const RESTORE_LIMIT: time::Duration = time::Duration::hours(24);

fn key_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "pitr_configs",
        column: "key",
        row,
    }
}

/// An instance's WAL-G storage path within its target.
pub fn storage_path(workspace: Id, instance: Id) -> String {
    format!("{workspace}/{instance}/wal-g")
}

/// WAL-G storage as the database container reaches it.
pub enum Storage {
    S3 {
        /// `s3://bucket/path`
        prefix: String,
        endpoint: String,
        region: String,
        access_key_id: String,
        secret: Zeroizing<String>,
        path_style: bool,
    },
    /// Under the node's backup directory (`<workspace>/<instance>/wal-g`).
    File { local_dir: String },
}

/// The WAL-G storage of `instance`'s WAL in `store`, for a database
/// container in `environment` on `node`.
pub fn storage(
    store: Store,
    workspace: Id,
    instance: Id,
    environment: Id,
    node: Option<Id>,
) -> AppResult<Storage> {
    let path = storage_path(workspace, instance);
    Ok(match store {
        Store::Remote(t) => Storage::S3 {
            prefix: format!("s3://{}/{}{path}", t.bucket, t.prefix),
            endpoint: t.endpoint.as_str().trim_end_matches('/').to_owned(),
            region: t.region,
            access_key_id: t.access_key_id,
            secret: t.secret,
            path_style: t.provider != super::s3::Provider::S3,
        },
        Store::Private(b) => {
            if b.environment != environment {
                return Err(AppError::Conflict(
                    "the database archives WAL itself, and reaches a self-hosted bucket only in its own environment: choose one there"
                        .into(),
                ));
            }
            Storage::S3 {
                prefix: format!("s3://{}/{}{path}", b.bucket, b.prefix),
                endpoint: format!("http://{}.modula.internal:{}", b.service_name, b.port),
                region: b.region,
                access_key_id: b.access_key_id,
                secret: b.secret,
                path_style: true,
            }
        }
        Store::Disk { node: disk } => {
            if node.is_some_and(|n| n != disk) {
                return Err(AppError::Conflict(
                    "WAL goes to the disk of the server running the database: choose that server's disk"
                        .into(),
                ));
            }
            Storage::File { local_dir: path }
        }
    })
}

/// WAL-G's settings for `storage`, encrypted with `key_hex` (32 bytes).
pub fn env(storage: &Storage, key_hex: &str) -> Vec<(String, String)> {
    let mut out = vec![
        ("WALG_LIBSODIUM_KEY".to_owned(), key_hex.to_owned()),
        ("WALG_LIBSODIUM_KEY_TRANSFORM".to_owned(), "hex".to_owned()),
        ("WALG_COMPRESSION_METHOD".to_owned(), "lz4".to_owned()),
    ];
    match storage {
        Storage::S3 {
            prefix,
            endpoint,
            region,
            access_key_id,
            secret,
            path_style,
        } => out.extend([
            ("WALG_S3_PREFIX".to_owned(), prefix.clone()),
            ("AWS_ENDPOINT".to_owned(), endpoint.clone()),
            ("AWS_REGION".to_owned(), region.clone()),
            ("AWS_S3_FORCE_PATH_STYLE".to_owned(), path_style.to_string()),
            ("AWS_ACCESS_KEY_ID".to_owned(), access_key_id.clone()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), secret.to_string()),
        ]),
        Storage::File { .. } => out.push(("WALG_FILE_PREFIX".to_owned(), CONTAINER_DIR.to_owned())),
    }
    out
}

/// `recovery_target_time` as Postgres reads it.
pub fn recovery_time(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}+00",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// A fresh WAL-G key: 32 random bytes, hex.
pub fn generate_key() -> AppResult<Zeroizing<String>> {
    let bytes: [u8; 32] = modula_crypto::random_bytes().map_err(anyhow::Error::from)?;
    Ok(Zeroizing::new(hex::encode(bytes)))
}

pub async fn seal_key(
    state: &AppState,
    workspace: Id,
    instance: Id,
    key: &str,
) -> AppResult<Vec<u8>> {
    let row = instance.to_string();
    state
        .keys
        .seal(&state.db, key.as_bytes(), key_aad(workspace, &row))
        .await
}

async fn open_key(
    state: &AppState,
    workspace: Id,
    instance: Id,
    sealed: &[u8],
) -> AppResult<Zeroizing<String>> {
    let row = instance.to_string();
    let plain = state
        .keys
        .open(&state.db, sealed, key_aad(workspace, &row))
        .await?;
    Ok(Zeroizing::new(
        std::str::from_utf8(&plain)
            .map_err(anyhow::Error::from)?
            .to_owned(),
    ))
}

/// The workspace of an instance.
async fn workspace_of(state: &AppState, instance: Id) -> AppResult<Id> {
    Ok(sqlx::query_scalar(
        "SELECT p.workspace_id FROM service_instances i JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?)
}

/// WAL-G settings for `source`'s storage (its key and target), and the
/// node mount they need.
async fn settings(
    state: &AppState,
    source: Id,
    target: Id,
    environment: Id,
    node: Id,
) -> AppResult<(Vec<(String, String)>, pb::Pitr)> {
    let workspace = workspace_of(state, source).await?;
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT key_sealed FROM pitr_configs WHERE service_instance_id = $1")
            .bind(source)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| {
                AppError::Conflict("no point-in-time recovery for this database".into())
            })?;
    let key = open_key(state, workspace, source, &sealed).await?;
    let (_, store) = load_target(state, target).await?;
    let storage = storage(store, workspace, source, environment, Some(node))?;
    let mount = pb::Pitr {
        local_dir: match &storage {
            Storage::File { local_dir } => local_dir.clone(),
            Storage::S3 { .. } => String::new(),
        },
    };
    Ok((env(&storage, &key), mount))
}

/// What a Postgres workload of `instance` on `node` needs for PITR: its
/// WAL-G settings (plus `MODULA_PITR` to archive, and the restore it runs
/// first, if any) and the node's mounts. None without PITR.
pub async fn workload(
    state: &AppState,
    instance: Id,
    environment: Id,
    node: Id,
) -> AppResult<Option<(Vec<(String, String)>, pb::Pitr)>> {
    let own: Option<Id> = sqlx::query_scalar(
        "SELECT target_id FROM pitr_configs WHERE service_instance_id = $1 AND enabled",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let restore: Option<(Id, Id, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT source_instance_id, target_id, base_backup, target_time FROM pitr_restores
         WHERE instance_id = $1 AND status = 'running'",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let mut out = match (&restore, own) {
        (Some((source, target, _, _)), _) => {
            settings(state, *source, *target, environment, node).await?
        }
        (None, Some(target)) => settings(state, instance, target, environment, node).await?,
        (None, None) => return Ok(None),
    };
    // A restored copy never archives into its source's storage (it would
    // fork the source's history); a database restored over itself does.
    if own.is_some() && restore.as_ref().is_none_or(|r| r.0 == instance) {
        out.0.push(("MODULA_PITR".into(), "1".into()));
    }
    if let Some((_, _, base, at)) = restore {
        out.0
            .push(("MODULA_PITR_RESTORE_TIME".into(), recovery_time(at)));
        out.0.push(("MODULA_PITR_RESTORE_BACKUP".into(), base));
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------- base backups

/// The node running an instance's active deployment.
async fn live(state: &AppState, instance: Id) -> AppResult<(Id, Id)> {
    let live: Option<(Id, Option<Id>)> = sqlx::query_as(
        "SELECT id, node_id FROM deployments WHERE service_instance_id = $1 AND status = 'active'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    match live {
        Some((d, Some(n))) => Ok((d, n)),
        _ => Err(AppError::Conflict("the database is not running".into())),
    }
}

/// Take a base backup now. Conflict if a backup of the instance is running.
pub async fn start_base(
    state: &AppState,
    instance: Id,
    kind: Kind,
    created_by: Option<Id>,
) -> AppResult<Id> {
    let (target,): (Id,) = sqlx::query_as(
        "SELECT target_id FROM pitr_configs WHERE service_instance_id = $1 AND enabled",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::Conflict("point-in-time recovery is off".into()))?;
    let (deployment, node) = live(state, instance).await?;
    let workspace = workspace_of(state, instance).await?;
    let id = Id::new();
    let inserted: Option<Id> = sqlx::query_scalar(
        "INSERT INTO backups (id, service_instance_id, target_id, node_id, kind, engine, method,
                              object_key, created_by)
         VALUES ($1, $2, $3, $4, $5, 'postgres', 'walg', $6, $7)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(instance)
    .bind(target)
    .bind(node)
    .bind(match kind {
        Kind::Scheduled => "scheduled",
        Kind::Manual => "manual",
    })
    .bind(storage_path(workspace, instance))
    .bind(created_by)
    .fetch_optional(&state.db)
    .await?;
    if inserted.is_none() {
        return Err(AppError::Conflict("a backup is already running".into()));
    }
    let req = pb::WalgRequest {
        request_id: id.to_string(),
        deployment_id: deployment.to_string(),
        op: pb::WalgOp::WalgBackupPush as i32,
        backup_name: String::new(),
    };
    if !state
        .hub
        .send(node, pb::server_message::Msg::Walg(req))
        .await
    {
        super::run::fail(state, id, "the server running the database is offline").await?;
        return Err(AppError::Conflict(
            "the server running the database is offline".into(),
        ));
    }
    Ok(id)
}

/// A node's WAL-G result: a base backup taken (or not), or a prune done.
pub async fn on_result(state: &AppState, node: Id, r: pb::WalgResult) -> anyhow::Result<()> {
    let Ok(id) = r.request_id.parse::<Id>() else {
        return Ok(());
    };
    let error: String = r.error.chars().take(500).collect();
    match pb::WalgOp::try_from(r.op) {
        Ok(pb::WalgOp::WalgBackupPush) if r.ok => {
            let finished = OffsetDateTime::from_unix_timestamp(r.finish_unix)
                .unwrap_or_else(|_| OffsetDateTime::now_utc());
            sqlx::query(
                "UPDATE backups SET status = 'completed', walg_name = $3, size_bytes = $4,
                        finished_at = $5
                 WHERE id = $1 AND node_id = $2 AND status = 'running' AND method = 'walg'",
            )
            .bind(id)
            .bind(node)
            .bind(&r.backup_name)
            .bind(i64::try_from(r.size_bytes).unwrap_or(i64::MAX))
            .bind(finished)
            .execute(&state.db)
            .await?;
            tracing::info!(backup = %id, name = r.backup_name, "base backup completed");
        }
        Ok(pb::WalgOp::WalgBackupPush) => {
            let mut tx = state.db.begin().await?;
            let instance: Option<Option<Id>> = sqlx::query_scalar(
                "UPDATE backups SET status = 'failed', error = $3, finished_at = now()
                 WHERE id = $1 AND node_id = $2 AND status = 'running' AND method = 'walg'
                 RETURNING service_instance_id",
            )
            .bind(id)
            .bind(node)
            .bind(&error)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(Some(instance)) = instance {
                super::run::alert_failed(&mut tx, id, instance).await?;
            }
            tx.commit().await?;
            tracing::warn!(backup = %id, error, "base backup failed");
        }
        // The id is the oldest base backup kept: older ones and their WAL
        // are gone. Only from the node running that instance's database.
        Ok(pb::WalgOp::WalgDeleteBefore) if r.ok => {
            sqlx::query(
                "UPDATE backups b SET status = 'deleted', deleted_at = now()
                 FROM backups k
                 WHERE k.id = $1 AND b.service_instance_id = k.service_instance_id
                   AND b.target_id = k.target_id AND b.method = 'walg'
                   AND b.status = 'completed' AND b.finished_at < k.finished_at
                   AND EXISTS (SELECT 1 FROM deployments d
                               WHERE d.service_instance_id = k.service_instance_id
                                 AND d.status = 'active' AND d.node_id = $2)",
            )
            .bind(id)
            .bind(node)
            .execute(&state.db)
            .await?;
        }
        _ => tracing::warn!(error, "pruning base backups failed"),
    }
    Ok(())
}

/// The base backup that must stay for `window_days` of history, when older
/// ones can go: the newest one finished before the window starts.
pub fn prune_point<T: Copy>(
    backups: &[(T, OffsetDateTime)],
    now: OffsetDateTime,
    window_days: i32,
) -> Option<T> {
    let start = now - time::Duration::days(i64::from(window_days));
    let mut before: Vec<&(T, OffsetDateTime)> =
        backups.iter().filter(|(_, at)| *at <= start).collect();
    before.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    match before.as_slice() {
        [keep, _, ..] => Some(keep.0),
        _ => None,
    }
}

/// From the periodic backup tick: due base backups, pruning, restores.
pub async fn tick(state: &AppState) -> anyhow::Result<()> {
    // Daily at the hour, and right away while there is none yet (retried
    // every ten minutes, e.g. while the redeploy that turned it on runs).
    let due: Vec<Id> = sqlx::query_scalar(concat!(
        "UPDATE pitr_configs c SET last_scheduled_at = now() WHERE enabled AND ((",
        super::schedule::due!(),
        ") OR (NOT EXISTS (SELECT 1 FROM backups b WHERE b.service_instance_id = c.service_instance_id
                            AND b.target_id = c.target_id AND b.method = 'walg'
                            AND b.status = 'completed')
               AND NOT EXISTS (SELECT 1 FROM backups b WHERE b.service_instance_id = c.service_instance_id
                            AND b.method = 'walg' AND b.started_at > now() - interval '10 minutes')))
         RETURNING service_instance_id"
    ))
    .fetch_all(&state.db)
    .await?;
    for instance in due {
        if let Err(e) = start_base(state, instance, Kind::Scheduled, None).await {
            tracing::debug!(%instance, error = ?e, "no base backup now");
        }
    }
    prune(state).await?;
    check_restores(state, None).await?;
    Ok(())
}

async fn prune(state: &AppState) -> anyhow::Result<()> {
    let rows: Vec<(Id, Id, i32, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT c.service_instance_id, b.id, c.window_days, b.walg_name, b.finished_at
         FROM pitr_configs c JOIN backups b ON b.service_instance_id = c.service_instance_id
              AND b.target_id = c.target_id AND b.method = 'walg' AND b.status = 'completed'
         WHERE c.enabled AND b.walg_name IS NOT NULL AND b.finished_at IS NOT NULL",
    )
    .fetch_all(&state.db)
    .await?;
    /// An instance's window and its base backups (with WAL-G's names).
    #[derive(Default)]
    struct Group {
        days: i32,
        backups: Vec<(Id, OffsetDateTime)>,
        names: HashMap<Id, String>,
    }
    let mut groups: HashMap<Id, Group> = HashMap::new();
    for (instance, id, days, name, at) in rows {
        let g = groups.entry(instance).or_default();
        g.days = days;
        g.backups.push((id, at));
        g.names.insert(id, name);
    }
    let now = OffsetDateTime::now_utc();
    for (
        instance,
        Group {
            days,
            backups,
            names,
        },
    ) in groups
    {
        let Some(keep) = prune_point(&backups, now, days) else {
            continue;
        };
        let Ok((deployment, node)) = live(state, instance).await else {
            continue;
        };
        let req = pb::WalgRequest {
            request_id: keep.to_string(),
            deployment_id: deployment.to_string(),
            op: pb::WalgOp::WalgDeleteBefore as i32,
            backup_name: names[&keep].clone(),
        };
        state
            .hub
            .send(node, pb::server_message::Msg::Walg(req))
            .await;
    }
    Ok(())
}

// ---------------------------------------------------------------- window

/// The span a restore can reach: from the oldest base backup to the last
/// WAL segment archived (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
}

/// The database's recovery and archiving state (the node's `walStatus`), if
/// it is running.
pub async fn wal_status(state: &AppState, instance: Id) -> Option<Value> {
    let live = crate::api::database::live(state, instance).await.ok()?;
    crate::api::database::call(
        state,
        &live,
        "walStatus",
        Value::Null,
        Duration::from_secs(10),
    )
    .await
    .ok()
}

/// The recoverable window of an instance with PITR in `target`.
pub async fn window(
    state: &AppState,
    instance: Id,
    target: Id,
    status: Option<&Value>,
) -> AppResult<Option<Window>> {
    let (oldest, newest): (Option<OffsetDateTime>, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT min(finished_at), max(finished_at) FROM backups
         WHERE service_instance_id = $1 AND target_id = $2 AND method = 'walg'
           AND status = 'completed'",
    )
    .bind(instance)
    .bind(target)
    .fetch_one(&state.db)
    .await?;
    let (Some(from), Some(newest)) = (oldest, newest) else {
        return Ok(None);
    };
    let archived = status
        .filter(|s| s["archiveMode"] == "on")
        .and_then(|s| s["lastArchivedAt"].as_f64())
        .and_then(|t| OffsetDateTime::from_unix_timestamp(t as i64).ok());
    Ok(Some(Window {
        from,
        to: archived.map_or(newest, |a| a.max(newest)),
    }))
}

/// The newest base backup of `instance` in `target` finished by `at`.
pub async fn base_for(
    state: &AppState,
    instance: Id,
    target: Id,
    at: OffsetDateTime,
) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT walg_name FROM backups
         WHERE service_instance_id = $1 AND target_id = $2 AND method = 'walg'
           AND status = 'completed' AND finished_at <= $3 AND walg_name IS NOT NULL
         ORDER BY finished_at DESC LIMIT 1",
    )
    .bind(instance)
    .bind(target)
    .bind(at)
    .fetch_optional(&state.db)
    .await?)
}

// ---------------------------------------------------------------- restores

/// Settle running restores (of `instance`, or all): done once the restored
/// database is out of recovery, failed when its deployment did not come up.
pub async fn check_restores(state: &AppState, instance: Option<Id>) -> anyhow::Result<()> {
    let rows: Vec<(Id, Id, OffsetDateTime, Option<String>)> = sqlx::query_as(
        "SELECT r.id, r.instance_id, r.started_at,
                (SELECT d.status FROM deployments d WHERE d.service_instance_id = r.instance_id
                   AND d.created_at >= r.started_at ORDER BY d.created_at DESC LIMIT 1)
         FROM pitr_restores r
         WHERE r.status = 'running'
           AND ($1::id IS NULL OR r.instance_id = $1 OR r.source_instance_id = $1)",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await?;
    for (id, restored, started, deployment) in rows {
        let outcome = match deployment.as_deref() {
            Some("failed" | "crashed" | "cancelled" | "removed") => Some(Err(
                "the restored database did not start: see its deployment logs",
            )),
            Some("active") => match wal_status(state, restored).await {
                Some(s) if s["inRecovery"] == false => Some(Ok(())),
                _ => None,
            },
            _ => None,
        };
        let outcome = outcome.or_else(|| {
            (OffsetDateTime::now_utc() - started > RESTORE_LIMIT)
                .then_some(Err("the restore did not finish within 24 hours"))
        });
        let Some(outcome) = outcome else { continue };
        sqlx::query(
            "UPDATE pitr_restores SET status = $2, error = $3, finished_at = now()
             WHERE id = $1 AND status = 'running'",
        )
        .bind(id)
        .bind(if outcome.is_ok() {
            "completed"
        } else {
            "failed"
        })
        .bind(outcome.err())
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn remote() -> Store {
        Store::Remote(super::super::s3::Target {
            provider: super::super::s3::Provider::R2,
            endpoint: "https://acc.r2.cloudflarestorage.com".parse().unwrap(),
            region: "auto".into(),
            bucket: "backups".into(),
            prefix: "team/".into(),
            access_key_id: "AK".into(),
            secret: Zeroizing::new("SK".into()),
        })
    }

    #[test]
    fn walg_settings_follow_the_store() {
        let (ws, inst, env_id, node) = (Id::new(), Id::new(), Id::new(), Id::new());
        let s = storage(remote(), ws, inst, env_id, Some(node)).unwrap();
        let vars: HashMap<String, String> = env(&s, &"ab".repeat(32)).into_iter().collect();
        assert_eq!(
            vars["WALG_S3_PREFIX"],
            format!("s3://backups/team/{ws}/{inst}/wal-g")
        );
        assert_eq!(vars["AWS_ENDPOINT"], "https://acc.r2.cloudflarestorage.com");
        assert_eq!(vars["AWS_REGION"], "auto");
        assert_eq!(vars["AWS_S3_FORCE_PATH_STYLE"], "true");
        assert_eq!(vars["AWS_SECRET_ACCESS_KEY"], "SK");
        assert_eq!(vars["WALG_LIBSODIUM_KEY"].len(), 64);
        assert_eq!(vars["WALG_LIBSODIUM_KEY_TRANSFORM"], "hex");
        assert!(!vars.contains_key("WALG_FILE_PREFIX"));

        // The server's disk: only for a database on that server.
        let disk = storage(Store::Disk { node }, ws, inst, env_id, Some(node)).unwrap();
        let Storage::File { local_dir } = &disk else {
            panic!("file storage");
        };
        assert_eq!(local_dir, &format!("{ws}/{inst}/wal-g"));
        let vars: HashMap<String, String> = env(&disk, "00").into_iter().collect();
        assert_eq!(vars["WALG_FILE_PREFIX"], CONTAINER_DIR);
        assert!(!vars.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(storage(Store::Disk { node }, ws, inst, env_id, Some(Id::new())).is_err());

        // A self-hosted bucket: by name, in the database's environment only.
        let bucket = |environment| {
            Store::Private(super::super::store::PrivateBucket {
                region: "us-east-1".into(),
                bucket: "bucket".into(),
                access_key_id: "AK".into(),
                secret: Zeroizing::new("SK".into()),
                prefix: "modula-backups/".into(),
                deployment: Id::new(),
                node,
                environment,
                port: 9000,
                mesh_ip: None,
                service_name: "files".into(),
            })
        };
        let s = storage(bucket(env_id), ws, inst, env_id, Some(node)).unwrap();
        let vars: HashMap<String, String> = env(&s, "00").into_iter().collect();
        assert_eq!(vars["AWS_ENDPOINT"], "http://files.modula.internal:9000");
        assert_eq!(vars["AWS_S3_FORCE_PATH_STYLE"], "true");
        assert!(storage(bucket(Id::new()), ws, inst, env_id, Some(node)).is_err());
    }

    #[test]
    fn recovery_times_are_postgres_timestamps() {
        assert_eq!(
            recovery_time(datetime!(2026-10-24 05:12:09 +02:00)),
            "2026-10-24 03:12:09+00"
        );
        assert_eq!(generate_key().unwrap().len(), 64);
        assert_ne!(*generate_key().unwrap(), *generate_key().unwrap());
    }

    #[test]
    fn pruning_keeps_the_backup_that_covers_the_window() {
        let now = datetime!(2026-10-24 04:00 UTC);
        let daily: Vec<(i64, OffsetDateTime)> = (0..10)
            .map(|i| (i, now - time::Duration::days(i) - time::Duration::hours(1)))
            .collect();
        // 7 days back is 2026-10-17 04:00: backup 7 (10-17 03:00) covers it,
        // so 8 and 9 go.
        assert_eq!(prune_point(&daily, now, 7), Some(7));
        assert_eq!(prune_point(&daily[..8], now, 7), None, "nothing older");
        assert_eq!(prune_point(&daily, now, 35), None);
        assert_eq!(prune_point::<i64>(&[], now, 7), None);
    }
}
