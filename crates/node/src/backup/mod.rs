//! Database backups and restores, run inside the database's container.
//!
//! Backup: the engine's dump command streams to stdout, is age-encrypted to
//! the workspace's recipient as it streams, and goes to one of three stores:
//!
//! - S3-compatible storage the control plane reaches: parts go straight up
//!   through presigned URLs, and the node never holds credentials;
//! - a self-hosted bucket service, which only the private network reaches:
//!   this node gets its credentials and does the multipart upload itself;
//! - this server's disk, under the backup directory
//!   (`/var/lib/modula/backups/<workspace>/<instance>/…`).
//!
//! Restore: the reverse, into the restore command's stdin. Plaintext never
//! touches the node's disk.
//!
//! Results go through the session's outbox, so one that finishes while the
//! control plane is unreachable is reported after reconnecting.

pub mod engines;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use anyhow::Context;
use modula_backup::net::{self, GuardedResolver};
use modula_backup::s3;
use modula_proto::node::v1 as pb;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::runtime::Runtime;

/// A dump or restore that takes longer than this is given up on.
const TIME_LIMIT: Duration = Duration::from_secs(12 * 3600);
/// Where local-disk backups live unless `--backup-dir` says otherwise.
pub const DEFAULT_DIR: &str = "/var/lib/modula/backups";
/// Parts of an upload to a self-hosted bucket (S3 allows 10 000).
const PRIVATE_PARTS: u16 = 10_000;

static DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Set the backup directory (`modula-node run --backup-dir`; tests use a
/// temporary one).
pub fn set_dir(dir: PathBuf) {
    *DIR.write().expect("backup dir lock") = Some(dir);
}

pub fn dir() -> PathBuf {
    DIR.read()
        .expect("backup dir lock")
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
}

fn http() -> anyhow::Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // No read timeout: a restore stops reading while the database is busy
        // (e.g. building an index); TIME_LIMIT bounds the whole transfer.
        .tcp_keepalive(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(GuardedResolver::new(
            reachable,
            "address backups may use",
        )))
        .no_proxy()
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Storage hosts are user input, validated by the control plane (which
/// allows private endpoints only on the operator's opt-in), but DNS answers
/// can differ here: never let them reach this server's link-local addresses,
/// where cloud metadata services live, nor its loopback services unless this
/// node opts in too (`MODULA_BACKUP_ALLOW_PRIVATE_ENDPOINTS=1`).
fn reachable(ip: IpAddr) -> bool {
    static LOOPBACK_OK: OnceLock<bool> = OnceLock::new();
    net::backup_ip(ip, true)
        && (!net::loopback(ip)
            || ALLOW_LOOPBACK.load(Ordering::Relaxed)
            || *LOOPBACK_OK.get_or_init(net::private_endpoints_from_env))
}

static ALLOW_LOOPBACK: AtomicBool = AtomicBool::new(false);

/// Allow loopback storage endpoints (tests, which run their storage on
/// 127.0.0.1; operators set `MODULA_BACKUP_ALLOW_PRIVATE_ENDPOINTS=1`).
pub fn allow_loopback_endpoints(allow: bool) {
    ALLOW_LOOPBACK.store(allow, Ordering::Relaxed);
}

/// Literal IP hosts skip the resolver, so they are checked up front.
fn check_url(raw: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(raw).context("invalid storage URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "https" | "http"),
        "invalid storage URL"
    );
    let host = url.host_str().context("invalid storage URL")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        anyhow::ensure!(
            reachable(ip),
            "the storage URL points at a link-local or loopback address"
        );
    }
    Ok(())
}

/// A path under the backup directory, as the control plane names them:
/// `/`-separated segments of letters, digits, `.`, `-` and `_`, none of
/// them `.` or `..`.
pub fn local_file(rel: &str) -> anyhow::Result<PathBuf> {
    let ok = !rel.is_empty()
        && rel.len() <= 512
        && rel.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        });
    anyhow::ensure!(ok, "invalid backup path");
    Ok(dir().join(rel))
}

/// A self-hosted bucket as a signing target: at its given address, or at
/// its container's address on this server.
async fn private_target(runtime: &dyn Runtime, a: &pb::S3Access) -> anyhow::Result<s3::Target> {
    let endpoint = if a.endpoint.is_empty() {
        let container = runtime
            .containers()
            .await?
            .into_iter()
            .find(|c| c.deployment_id == a.deployment_id && c.running)
            .context("the bucket is not running on this server")?;
        let ip = runtime
            .address(&container.id, &a.environment_id)
            .await?
            .context("the bucket has no address yet")?;
        let host = match ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        format!("http://{host}:{}", a.port)
    } else {
        a.endpoint.clone()
    };
    check_url(&endpoint)?;
    Ok(s3::Target {
        provider: s3::Provider::Minio,
        endpoint: endpoint.parse().context("invalid bucket endpoint")?,
        region: a.region.clone(),
        bucket: a.bucket.clone(),
        prefix: String::new(),
        access_key_id: a.access_key_id.clone(),
        secret: Zeroizing::new(a.secret_access_key.clone()),
    })
}

/// The running container of a deployment and its environment.
async fn container(
    runtime: &dyn Runtime,
    deployment_id: &str,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let container = runtime
        .containers()
        .await?
        .into_iter()
        .find(|c| c.deployment_id == deployment_id && c.running)
        .context("the database is not running on this server")?;
    let env = runtime.container_env(&container.id).await?;
    Ok((container.id, env))
}

fn engine(raw: i32) -> anyhow::Result<pb::DatabaseEngine> {
    pb::DatabaseEngine::try_from(raw).map_err(|_| anyhow::anyhow!("unknown database engine"))
}

fn result(
    id: String,
    restore: bool,
    outcome: anyhow::Result<modula_backup::stream::Uploaded>,
) -> pb::NodeMessage {
    let result = match outcome {
        Ok(up) => pb::BackupResult {
            id,
            restore,
            ok: true,
            size_bytes: up.size_bytes,
            error: String::new(),
            etags: up.etags,
        },
        Err(e) => pb::BackupResult {
            id,
            restore,
            ok: false,
            // Stored on the backup row and shown in the dashboard.
            error: format!("{e:#}").chars().take(500).collect(),
            ..Default::default()
        },
    };
    pb::NodeMessage {
        msg: Some(pb::node_message::Msg::BackupResult(result)),
    }
}

async fn limited<T>(work: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(TIME_LIMIT, work)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "timed out after {} hours",
                TIME_LIMIT.as_secs() / 3600
            ))
        })
}

type Plaintext = futures::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>>;

/// Encrypt into `<path>.partial`, then rename: a file under its final name
/// is always complete.
async fn write_local(
    plaintext: Plaintext,
    recipient: &modula_backup::keys::Recipient,
    path: &Path,
) -> anyhow::Result<u64> {
    let parent = path.parent().context("invalid backup path")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let partial = path.with_extension("partial");
    let written = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)
            .await
            .with_context(|| format!("cannot write {}", partial.display()))?;
        let n = modula_backup::stream::encrypt_to(plaintext, recipient, &mut file).await?;
        file.sync_all().await?;
        anyhow::Ok(n)
    }
    .await;
    match written {
        Ok(n) => {
            tokio::fs::rename(&partial, path).await?;
            Ok(n)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(e)
        }
    }
}

/// A multipart upload to a self-hosted bucket, aborted if anything fails.
async fn upload_private(
    target: &s3::Target,
    key: &str,
    plaintext: Plaintext,
    recipient: &modula_backup::keys::Recipient,
    part_size: usize,
) -> anyhow::Result<modula_backup::stream::Uploaded> {
    let http = http()?;
    let upload = target.create_multipart(http, key).await?;
    let uploaded = async {
        let urls = target.presign_parts(key, &upload, PRIVATE_PARTS)?;
        let up =
            modula_backup::stream::encrypt_and_upload(http, plaintext, recipient, &urls, part_size)
                .await?;
        target
            .complete_multipart(http, key, &upload, &up.etags)
            .await?;
        anyhow::Ok(up)
    }
    .await;
    if uploaded.is_err()
        && let Err(e) = target.abort_multipart(http, key, &upload).await
    {
        tracing::warn!(error = format!("{e:#}"), "could not abort a backup upload");
    }
    // The server completes nothing: no ETags to report.
    uploaded.map(|up| modula_backup::stream::Uploaded {
        size_bytes: up.size_bytes,
        etags: Vec::new(),
    })
}

pub async fn backup(
    runtime: Arc<dyn Runtime>,
    req: pb::BackupRequest,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    tracing::info!(backup = %req.backup_id, deployment = %req.deployment_id, "backup started");
    let outcome = limited(async {
        let recipient = modula_backup::keys::parse_recipient(&req.age_recipient)?;
        req.part_urls.iter().try_for_each(|u| check_url(u))?;
        let local = (!req.local_path.is_empty())
            .then(|| local_file(&req.local_path))
            .transpose()?;
        let private = match &req.s3 {
            Some(a) => Some(private_target(runtime.as_ref(), a).await?),
            None => None,
        };
        let (container, env) = container(runtime.as_ref(), &req.deployment_id).await?;
        let exec = engines::dump(engine(req.engine)?, &env)?;
        let dump = runtime.exec_output(&container, exec).await?;
        let part_size = usize::try_from(req.part_size).context("invalid part size")?;
        if let Some(path) = local {
            let size_bytes = write_local(dump, &recipient, &path)
                .await
                .context("backup failed")?;
            return Ok(modula_backup::stream::Uploaded {
                size_bytes,
                etags: Vec::new(),
            });
        }
        if let Some(target) = private {
            return upload_private(&target, &req.object_key, dump, &recipient, part_size)
                .await
                .context("backup failed");
        }
        modula_backup::stream::encrypt_and_upload(
            http()?,
            dump,
            &recipient,
            &req.part_urls,
            part_size,
        )
        .await
        .context("backup failed")
    })
    .await;
    match &outcome {
        Ok(up) => tracing::info!(backup = %req.backup_id, bytes = up.size_bytes, "backup stored"),
        Err(e) => {
            tracing::warn!(backup = %req.backup_id, error = format!("{e:#}"), "backup failed")
        }
    }
    let _ = results.send(result(req.backup_id, false, outcome)).await;
}

/// The decrypted backup a restore request names, as it is read.
async fn open_backup(
    runtime: &dyn Runtime,
    req: &pb::RestoreRequest,
    identity: &modula_backup::keys::Identity,
) -> anyhow::Result<Plaintext> {
    if !req.local_path.is_empty() {
        let path = local_file(&req.local_path)?;
        let file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("cannot open the backup {}", path.display()))?;
        return modula_backup::stream::read_and_decrypt(file, identity).await;
    }
    let url = match &req.s3 {
        Some(a) => private_target(runtime, a)
            .await?
            .presign_get(&req.object_key)?,
        None => req.object_url.clone(),
    };
    check_url(&url)?;
    modula_backup::stream::download_and_decrypt(http()?, &url, identity).await
}

pub async fn restore(
    runtime: Arc<dyn Runtime>,
    req: pb::RestoreRequest,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    tracing::info!(restore = %req.restore_id, deployment = %req.deployment_id, "restore started");
    let outcome = limited(async {
        let identity = modula_backup::keys::parse_identity(&req.age_identity)?;
        let (container, env) = container(runtime.as_ref(), &req.deployment_id).await?;
        let exec = engines::restore(engine(req.engine)?, &env)?;
        let plaintext = open_backup(runtime.as_ref(), &req, &identity).await?;
        runtime
            .exec_input(&container, exec, plaintext)
            .await
            .context("restore failed")
    })
    .await;
    if let Err(e) = &outcome {
        tracing::warn!(restore = %req.restore_id, error = format!("{e:#}"), "restore failed");
    } else {
        tracing::info!(restore = %req.restore_id, "restore finished");
    }
    let outcome = outcome.map(|()| Default::default());
    let _ = results.send(result(req.restore_id, true, outcome)).await;
}

/// Delete one backup from a self-hosted bucket or this server's disk. One
/// that is already gone counts as deleted.
pub async fn delete(
    runtime: Arc<dyn Runtime>,
    req: pb::BackupDelete,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    let outcome = async {
        if !req.local_path.is_empty() {
            let path = local_file(&req.local_path)?;
            return match tokio::fs::remove_file(&path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(anyhow::Error::from(e).context("cannot delete the backup"))
                }
                _ => Ok(()),
            };
        }
        let access = req.s3.as_ref().context("nothing to delete")?;
        let target = private_target(runtime.as_ref(), access).await?;
        target.delete(http()?, &req.object_key).await
    }
    .await;
    if let Err(e) = &outcome {
        tracing::warn!(backup = %req.backup_id, error = format!("{e:#}"), "backup deletion failed");
    }
    let msg = pb::BackupDeleted {
        backup_id: req.backup_id,
        ok: outcome.is_ok(),
        error: outcome
            .err()
            .map(|e| format!("{e:#}").chars().take(500).collect())
            .unwrap_or_default(),
    };
    let _ = results
        .send(pb::NodeMessage {
            msg: Some(pb::node_message::Msg::BackupDeleted(msg)),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;

    #[test]
    fn metadata_addresses_are_refused() {
        assert!(check_url("https://abc.r2.cloudflarestorage.com/b/k?X-Amz-Signature=1").is_ok());
        assert!(check_url("http://10.0.0.5:9000/b/k").is_ok());
        for bad in [
            "http://169.254.169.254/latest",
            "http://[fe80::1]/x",
            "http://[::ffff:169.254.169.254]/x",
            "http://0.0.0.0:9000/b",
            "http://[fd00:ec2::254]/latest",
            "http://[64:ff9b::a9fe:a9fe]/latest",
            "http://127.0.0.1:2375/containers/json",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn local_paths_stay_under_the_backup_directory() {
        assert!(local_file("ws/inst/20261023T030000Z-x.dump.age").is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "a/./b",
            "a/b c",
            "a\\b",
        ] {
            assert!(local_file(bad).is_err(), "{bad:?}");
        }
    }

    /// A deployment's container on the fake runtime, created and started.
    async fn running(rt: &FakeRuntime, deployment: &str) {
        let w = pb::Workload {
            deployment_id: deployment.into(),
            environment_id: "env".into(),
            service_name: "db".into(),
            image: "postgres:17".into(),
            ..Default::default()
        };
        rt.ensure_network("env").await.unwrap();
        let id = rt.create(&w).await.unwrap();
        rt.start(&id).await.unwrap();
    }

    #[tokio::test]
    async fn local_disk_backups_round_trip() {
        let tmp = std::env::temp_dir().join(format!("modula-backups-{}", modula_id::Id::new()));
        set_dir(tmp.clone());
        let rt = Arc::new(FakeRuntime::new());
        running(&rt, "dep1").await;
        let dump: Vec<u8> = b"PGDMP local "
            .iter()
            .copied()
            .cycle()
            .take(200_000)
            .collect();
        *rt.exec_stdout.lock().unwrap() = dump.clone();
        let (recipient, identity) = modula_backup::keys::generate();
        let (tx, mut rx) = mpsc::channel(4);
        let rel = "ws1/inst1/20261023T030000Z-b1.dump.age";

        backup(
            rt.clone(),
            pb::BackupRequest {
                backup_id: "b1".into(),
                deployment_id: "dep1".into(),
                engine: pb::DatabaseEngine::EnginePostgres as i32,
                age_recipient: recipient,
                part_size: 16 << 20,
                local_path: rel.into(),
                ..Default::default()
            },
            tx.clone(),
        )
        .await;
        let Some(pb::node_message::Msg::BackupResult(r)) = rx.recv().await.unwrap().msg else {
            panic!("no result");
        };
        assert!(r.ok, "{}", r.error);
        let path = tmp.join(rel);
        let stored = std::fs::read(&path).unwrap();
        assert_eq!(r.size_bytes, stored.len() as u64);
        assert!(!stored.windows(5).any(|w| w == b"PGDMP"), "encrypted");
        assert!(!path.with_extension("partial").exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Restore reads it back, decrypted, into the restore command.
        restore(
            rt.clone(),
            pb::RestoreRequest {
                restore_id: "r1".into(),
                deployment_id: "dep1".into(),
                engine: pb::DatabaseEngine::EnginePostgres as i32,
                age_identity: identity.to_string(),
                local_path: rel.into(),
                ..Default::default()
            },
            tx.clone(),
        )
        .await;
        let Some(pb::node_message::Msg::BackupResult(r)) = rx.recv().await.unwrap().msg else {
            panic!("no result");
        };
        assert!(r.ok && r.restore, "{}", r.error);
        let exec = rt.execs.lock().unwrap().last().unwrap().clone();
        assert_eq!(exec.cmd[0], "pg_restore");
        assert_eq!(exec.stdin.as_deref(), Some(&dump[..]));

        // Deleting removes the file; deleting again is fine.
        for _ in 0..2 {
            delete(
                rt.clone(),
                pb::BackupDelete {
                    backup_id: "b1".into(),
                    local_path: rel.into(),
                    ..Default::default()
                },
                tx.clone(),
            )
            .await;
            let Some(pb::node_message::Msg::BackupDeleted(d)) = rx.recv().await.unwrap().msg else {
                panic!("no result");
            };
            assert!(d.ok, "{}", d.error);
        }
        assert!(!path.exists());

        // A failed dump leaves nothing behind.
        rt.execs.lock().unwrap().clear();
        backup(
            rt.clone(),
            pb::BackupRequest {
                backup_id: "b2".into(),
                deployment_id: "missing".into(),
                engine: pb::DatabaseEngine::EnginePostgres as i32,
                age_recipient: modula_backup::keys::generate().0,
                part_size: 16 << 20,
                local_path: "ws1/inst1/b2.dump.age".into(),
                ..Default::default()
            },
            tx,
        )
        .await;
        let Some(pb::node_message::Msg::BackupResult(r)) = rx.recv().await.unwrap().msg else {
            panic!("no result");
        };
        assert!(!r.ok);
        let left: Vec<_> = std::fs::read_dir(tmp.join("ws1/inst1"))
            .map(|d| d.collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
