//! Point-in-time recovery for Postgres with WAL-G (Apache-2.0).
//!
//! Containers of a workload with `Workload.pitr` get this node's WAL-G
//! binary bind-mounted read-only at [`CONTAINER_BIN`] (the release ships it
//! next to modula-node; `--wal-g` points at it) and, for disk storage, their
//! WAL-G directory under the backup directory at [`CONTAINER_DIR`]. The
//! template's start script turns on archiving (`archive_command = wal-g
//! wal-push %p`) when the server sets `MODULA_PITR`, and restores from a
//! base backup when it sets `MODULA_PITR_RESTORE_*`.
//!
//! Base backups and their pruning run here on the server's request
//! ([`run`]), inside the container as the postgres user, so files WAL-G
//! writes to disk storage stay the server's to append to.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use futures::StreamExt;
use modula_proto::node::v1 as pb;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::runtime::{Exec, Runtime};

pub const CONTAINER_BIN: &str = "/usr/local/bin/wal-g";
pub const CONTAINER_DIR: &str = "/var/lib/modula-wal-g";

static BIN: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Set the WAL-G binary (`modula-node run --wal-g`).
pub fn set_bin(path: PathBuf) {
    *BIN.write().expect("wal-g lock") = Some(path);
}

/// This node's WAL-G binary, if it has one.
pub fn bin() -> Option<PathBuf> {
    BIN.read()
        .expect("wal-g lock")
        .clone()
        .filter(|p| p.is_file())
}

/// The host directory of a workload's disk storage (created if missing).
pub fn local_dir(rel: &str) -> anyhow::Result<PathBuf> {
    let dir = super::local_file(rel)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// `wal-g <args>` as the postgres user, connecting over the Unix socket.
fn command(env: &[(String, String)], args: &[&str]) -> Exec {
    let user = var(env, "POSTGRES_USER").unwrap_or("postgres");
    let db = var(env, "POSTGRES_DB").unwrap_or(user);
    let mut cmd = vec!["gosu".to_owned(), "postgres".to_owned(), "wal-g".to_owned()];
    cmd.extend(args.iter().map(|a| (*a).to_owned()));
    let mut extra = vec![
        "PGHOST=/var/run/postgresql".to_owned(),
        format!("PGUSER={user}"),
        format!("PGDATABASE={db}"),
    ];
    if let Some(p) = var(env, "POSTGRES_PASSWORD") {
        extra.push(format!("PGPASSWORD={p}"));
    }
    Exec { cmd, env: extra }
}

pub fn pgdata(env: &[(String, String)]) -> String {
    var(env, "PGDATA")
        .unwrap_or("/var/lib/postgresql/data")
        .to_owned()
}

/// The commands a request runs, in order.
pub fn commands(env: &[(String, String)], req: &pb::WalgRequest) -> anyhow::Result<Vec<Exec>> {
    Ok(match pb::WalgOp::try_from(req.op) {
        Ok(pb::WalgOp::WalgBackupPush) => vec![
            command(env, &["backup-push", &pgdata(env)]),
            command(env, &["backup-list", "--json", "--detail"]),
        ],
        Ok(pb::WalgOp::WalgDeleteBefore) => {
            anyhow::ensure!(
                !req.backup_name.is_empty()
                    && req
                        .backup_name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid base backup name"
            );
            vec![command(
                env,
                &[
                    "delete",
                    "before",
                    "FIND_FULL",
                    &req.backup_name,
                    "--confirm",
                ],
            )]
        }
        _ => anyhow::bail!("unknown WAL-G operation"),
    })
}

/// One entry of `wal-g backup-list --json --detail`.
#[derive(Debug, Deserialize)]
pub struct Listed {
    pub backup_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub start_time: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finish_time: time::OffsetDateTime,
    #[serde(default)]
    pub compressed_size: u64,
}

/// The newest base backup in a `backup-list` answer.
pub fn newest(json: &[u8]) -> anyhow::Result<Listed> {
    let list: Vec<Listed> =
        serde_json::from_slice(json).context("unexpected wal-g backup-list output")?;
    list.into_iter()
        .max_by_key(|b| b.finish_time)
        .context("wal-g lists no base backup")
}

async fn exec(runtime: &dyn Runtime, container: &str, e: Exec) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = runtime.exec_output(container, e).await?;
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

pub async fn run(
    runtime: Arc<dyn Runtime>,
    req: pb::WalgRequest,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    let outcome = super::limited(async {
        let (container, env) = super::container(runtime.as_ref(), &req.deployment_id).await?;
        let mut last = Vec::new();
        for e in commands(&env, &req)? {
            last = exec(runtime.as_ref(), &container, e).await?;
        }
        anyhow::Ok(last)
    })
    .await;
    let mut result = pb::WalgResult {
        request_id: req.request_id.clone(),
        op: req.op,
        ok: true,
        ..Default::default()
    };
    match outcome.and_then(|out| match pb::WalgOp::try_from(req.op) {
        Ok(pb::WalgOp::WalgBackupPush) => newest(&out).map(Some),
        _ => Ok(None),
    }) {
        Ok(Some(b)) => {
            result.backup_name = b.backup_name;
            result.start_unix = b.start_time.unix_timestamp();
            result.finish_unix = b.finish_time.unix_timestamp();
            result.size_bytes = b.compressed_size;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(request = %req.request_id, error = format!("{e:#}"), "wal-g failed");
            result.ok = false;
            result.error = format!("{e:#}").chars().take(500).collect();
        }
    }
    let _ = results
        .send(pb::NodeMessage {
            msg: Some(pb::node_message::Msg::WalgResult(result)),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Vec<(String, String)> {
        [
            ("POSTGRES_USER", "app"),
            ("POSTGRES_PASSWORD", "pw"),
            ("POSTGRES_DB", "shop"),
            ("PGDATA", "/var/lib/postgresql/data/pgdata"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
    }

    #[test]
    fn commands_run_as_postgres_over_the_socket() {
        let push = pb::WalgRequest {
            op: pb::WalgOp::WalgBackupPush as i32,
            ..Default::default()
        };
        let c = commands(&env(), &push).unwrap();
        assert_eq!(
            c[0].cmd,
            [
                "gosu",
                "postgres",
                "wal-g",
                "backup-push",
                "/var/lib/postgresql/data/pgdata"
            ]
        );
        assert!(c[0].env.contains(&"PGHOST=/var/run/postgresql".to_owned()));
        assert!(c[0].env.contains(&"PGUSER=app".to_owned()));
        assert!(c[0].env.contains(&"PGPASSWORD=pw".to_owned()));
        assert_eq!(c[1].cmd[3..], ["backup-list", "--json", "--detail"]);

        let prune = pb::WalgRequest {
            op: pb::WalgOp::WalgDeleteBefore as i32,
            backup_name: "base_000000010000000000000004".into(),
            ..Default::default()
        };
        let c = commands(&env(), &prune).unwrap();
        assert_eq!(
            c[0].cmd[3..],
            [
                "delete",
                "before",
                "FIND_FULL",
                "base_000000010000000000000004",
                "--confirm"
            ]
        );
        let evil = pb::WalgRequest {
            backup_name: "x --everything".into(),
            ..prune
        };
        assert!(commands(&env(), &evil).is_err());
        assert!(commands(&env(), &pb::WalgRequest::default()).is_err());
    }

    #[test]
    fn the_newest_listed_backup_is_reported() {
        // The shape of `wal-g backup-list --json --detail` (v3).
        let json = br#"[
          {"backup_name":"base_000000010000000000000002","time":"2026-10-23T03:00:05Z",
           "wal_file_name":"000000010000000000000002","start_time":"2026-10-23T03:00:00Z",
           "finish_time":"2026-10-23T03:00:05Z","hostname":"db","data_dir":"/var/lib/postgresql/data/pgdata",
           "pg_version":170000,"start_lsn":33554472,"finish_lsn":33554720,"is_permanent":false,
           "system_identifier":7400000000000000000,"uncompressed_size":30000000,"compressed_size":4000000},
          {"backup_name":"base_000000010000000000000006","time":"2026-10-24T03:00:07Z",
           "wal_file_name":"000000010000000000000006","start_time":"2026-10-24T03:00:00Z",
           "finish_time":"2026-10-24T03:00:07Z","hostname":"db","data_dir":"/var/lib/postgresql/data/pgdata",
           "pg_version":170000,"start_lsn":100663336,"finish_lsn":100663584,"is_permanent":false,
           "system_identifier":7400000000000000000,"uncompressed_size":31000000,"compressed_size":4100000}
        ]"#;
        let b = newest(json).unwrap();
        assert_eq!(b.backup_name, "base_000000010000000000000006");
        assert_eq!(b.compressed_size, 4_100_000);
        assert_eq!(
            b.finish_time.unix_timestamp() - b.start_time.unix_timestamp(),
            7
        );
        assert!(newest(b"[]").is_err());
        assert!(newest(b"not json").is_err());
    }
}
