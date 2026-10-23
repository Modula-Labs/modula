//! Database backups and restores, run inside the database's container.
//!
//! Backup: the engine's dump command streams to stdout, is age-encrypted to
//! the workspace's recipient as it streams, and goes straight to storage in
//! parts through presigned URLs. Restore: the reverse, into the restore
//! command's stdin. Plaintext never touches the node's disk, and the node
//! never holds storage credentials.
//!
//! Results go through the session's outbox, so one that finishes while the
//! control plane is unreachable is reported after reconnecting.

pub mod engines;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use modula_proto::node::v1 as pb;
use tokio::sync::mpsc;

use crate::runtime::Runtime;

/// A dump or restore that takes longer than this is given up on.
const TIME_LIMIT: Duration = Duration::from_secs(12 * 3600);

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
        .dns_resolver(Arc::new(GuardedResolver))
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Storage hosts are user input (validated by the control plane, but DNS
/// answers can differ here): never let them reach this server's link-local
/// addresses, where cloud metadata services live.
fn reachable(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(a) => !(a.is_link_local() || a.is_unspecified() || a.is_multicast()),
        IpAddr::V6(a) => {
            !(a.is_unspecified() || a.is_multicast() || (a.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| reachable(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no address backups may use").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
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
            "the storage URL points at a link-local address"
        );
    }
    Ok(())
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
            error: format!("{e:#}").chars().take(2000).collect(),
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

pub async fn backup(
    runtime: Arc<dyn Runtime>,
    req: pb::BackupRequest,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    tracing::info!(backup = %req.backup_id, deployment = %req.deployment_id, "backup started");
    let outcome = limited(async {
        let recipient = modula_backup::keys::parse_recipient(&req.age_recipient)?;
        req.part_urls.iter().try_for_each(|u| check_url(u))?;
        let (container, env) = container(runtime.as_ref(), &req.deployment_id).await?;
        let exec = engines::dump(engine(req.engine)?, &env)?;
        let dump = runtime.exec_output(&container, exec).await?;
        let part_size = usize::try_from(req.part_size).context("invalid part size")?;
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
        Ok(up) => tracing::info!(backup = %req.backup_id, bytes = up.size_bytes, "backup uploaded"),
        Err(e) => {
            tracing::warn!(backup = %req.backup_id, error = format!("{e:#}"), "backup failed")
        }
    }
    let _ = results.send(result(req.backup_id, false, outcome)).await;
}

pub async fn restore(
    runtime: Arc<dyn Runtime>,
    req: pb::RestoreRequest,
    results: mpsc::Sender<pb::NodeMessage>,
) {
    tracing::info!(restore = %req.restore_id, deployment = %req.deployment_id, "restore started");
    let outcome = limited(async {
        let identity = modula_backup::keys::parse_identity(&req.age_identity)?;
        check_url(&req.object_url)?;
        let (container, env) = container(runtime.as_ref(), &req.deployment_id).await?;
        let exec = engines::restore(engine(req.engine)?, &env)?;
        let plaintext =
            modula_backup::stream::download_and_decrypt(http()?, &req.object_url, &identity)
                .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_addresses_are_refused() {
        assert!(check_url("https://abc.r2.cloudflarestorage.com/b/k?X-Amz-Signature=1").is_ok());
        assert!(check_url("http://10.0.0.5:9000/b/k").is_ok());
        for bad in [
            "http://169.254.169.254/latest",
            "http://[fe80::1]/x",
            "http://[::ffff:169.254.169.254]/x",
            "http://0.0.0.0:9000/b",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }
}
