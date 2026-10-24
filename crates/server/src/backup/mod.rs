//! Database backups.
//!
//! A backup of a database service runs on the node that runs it, to one of
//! three stores (see `store`). For S3-compatible storage the control plane
//! reaches, the server starts a multipart upload, presigns its part URLs and
//! sends them with the workspace's age recipient; the node dumps, encrypts
//! and uploads, then reports the part ETags, and the server completes the
//! upload (see `run`). A self-hosted bucket service is only reachable over the
//! private network, so the node gets its credentials and uploads by itself;
//! a server's own disk is written by that server. Restores send a presigned
//! download URL (or the bucket's credentials, or the file's path) and the
//! workspace's age identity over the node's mTLS stream.
//!
//! Every workspace has its own age key pair (`backup_keys`), the identity
//! sealed with the workspace data key. The control plane's own database is
//! backed up by the server itself (see `control_plane`).
//!
//! Scheduling, retention and cleanup run from one periodic job (`schedule`).

pub mod control_plane;
pub mod pitr;
pub mod retention;
pub mod run;
pub mod s3;
pub mod schedule;
pub mod store;

use std::time::Duration;

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::jobs;
use crate::state::AppState;

pub use run::{on_deleted, on_result};
pub use store::{Store, load_target, target_aad};

/// Presigned part URLs sent with each backup request.
const PART_URLS: u16 = 1000;
const MIB: u64 = 1024 * 1024;
const MIN_PART: u64 = 16 * MIB;
const MAX_PART: u64 = 512 * MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Mysql,
    Redis,
    Mongo,
}

impl Engine {
    /// From a service's database template.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "postgres" => Self::Postgres,
            "mysql" => Self::Mysql,
            "redis" => Self::Redis,
            "mongo" => Self::Mongo,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Redis => "redis",
            Self::Mongo => "mongo",
        }
    }

    fn proto(self) -> pb::DatabaseEngine {
        match self {
            Self::Postgres => pb::DatabaseEngine::EnginePostgres,
            Self::Mysql => pb::DatabaseEngine::EngineMysql,
            Self::Redis => pb::DatabaseEngine::EngineRedis,
            Self::Mongo => pb::DatabaseEngine::EngineMongo,
        }
    }

    /// What the decrypted object is: a pg_dump archive, SQL, an RDB file or
    /// a gzipped mongodump archive.
    fn extension(self) -> &'static str {
        match self {
            Self::Postgres => "dump",
            Self::Mysql => "sql",
            Self::Redis => "rdb",
            Self::Mongo => "archive.gz",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Scheduled,
    Manual,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }
}

/// Parts have one fixed size (R2 requires it) and their URLs are presigned up
/// front, so they are sized from the previous backup with 4× headroom.
pub fn part_size(previous_bytes: Option<i64>) -> u64 {
    let previous = u64::try_from(previous_bytes.unwrap_or(0)).unwrap_or(0);
    let wanted = previous.saturating_mul(4) / u64::from(PART_URLS);
    (wanted.div_ceil(MIB) * MIB).clamp(MIN_PART, MAX_PART)
}

/// `20261023T030000Z`, so object names sort by time.
fn stamp(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

// ---------------------------------------------------------------- keys

fn key_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "backup_keys",
        column: "identity",
        row,
    }
}

/// The workspace's age recipient, creating its key pair on first use.
pub async fn recipient(state: &AppState, workspace: Id) -> AppResult<String> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT recipient FROM backup_keys WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_optional(&state.db)
            .await?;
    if let Some(r) = existing {
        return Ok(r);
    }
    let (recipient, identity) = modula_backup::keys::generate();
    let row = workspace.to_string();
    let sealed = state
        .keys
        .seal(&state.db, identity.as_bytes(), key_aad(workspace, &row))
        .await?;
    // Racing creators: the first insert wins and everyone reads that row back.
    sqlx::query(
        "INSERT INTO backup_keys (workspace_id, recipient, identity_sealed) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(workspace)
    .bind(&recipient)
    .bind(&sealed)
    .execute(&state.db)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT recipient FROM backup_keys WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_one(&state.db)
            .await?,
    )
}

/// The workspace's age identity (`AGE-SECRET-KEY-1…`).
pub async fn identity(state: &AppState, workspace: Id) -> AppResult<Zeroizing<String>> {
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT identity_sealed FROM backup_keys WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("workspace has no backup key")))?;
    let row = workspace.to_string();
    let plain = state
        .keys
        .open(&state.db, &sealed, key_aad(workspace, &row))
        .await?;
    let text = std::str::from_utf8(&plain).map_err(anyhow::Error::from)?;
    Ok(Zeroizing::new(text.to_owned()))
}

// ---------------------------------------------------------------- jobs

pub fn register(registry: jobs::Registry) -> jobs::Registry {
    registry
        .on(run::RUN_JOB, run::run_job)
        .on(run::FINISH_JOB, run::finish_job)
        .on(schedule::TICK_JOB, schedule::tick_job)
}

/// Enqueue the scheduling/retention tick every few minutes.
pub async fn schedule_loop(state: AppState, stop: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {
                let job = jobs::NewJob {
                    kind: schedule::TICK_JOB,
                    dedupe_key: Some(schedule::TICK_JOB),
                    max_attempts: Some(1),
                    ..Default::default()
                };
                match jobs::enqueue(&state.db, job).await {
                    Ok(_) => state.jobs_notify.notify_one(),
                    Err(e) => tracing::warn!(error = %e, "could not schedule backups"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_grow_with_the_database() {
        assert_eq!(part_size(None), MIN_PART);
        assert_eq!(part_size(Some(1_000_000)), MIN_PART);
        // 10 GiB last time: room for 40 GiB in 1000 parts.
        let p = part_size(Some(10 * 1024 * MIB as i64));
        assert_eq!(p, 41 * MIB);
        assert!(p * u64::from(PART_URLS) >= 40 * 1024 * MIB);
        assert_eq!(part_size(Some(i64::MAX)), MAX_PART);
    }

    #[test]
    fn stamps_sort_by_time() {
        let t = time::OffsetDateTime::from_unix_timestamp(1_792_724_400).unwrap();
        assert_eq!(stamp(t), "20261023T030000Z");
    }
}
