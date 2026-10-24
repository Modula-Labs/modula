//! Disk usage, reported every [`EVERY`]: free space of the filesystems
//! holding the node's data dir and Docker's data (where volumes live), and
//! each managed volume's size. `df` and `du` keep it dependency-free; `du`
//! walks every volume, so it runs rarely and under a timeout.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use modula_proto::node::v1 as pb;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

pub const EVERY: Duration = Duration::from_secs(300);
/// The first report comes this long after start (the server has it soon).
const FIRST_AFTER: Duration = Duration::from_secs(60);
const DU_TIMEOUT: Duration = Duration::from_secs(60);

/// Sends a report every [`EVERY`] until `stop`; dropped when the outbox is full.
pub async fn run(
    data_dir: PathBuf,
    runtime: Arc<dyn Runtime>,
    out: mpsc::Sender<pb::NodeMessage>,
    stop: CancellationToken,
) {
    let mut wait = FIRST_AFTER;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = EVERY;
        let r = report(&data_dir, runtime.as_ref()).await;
        if !r.filesystems.is_empty() || !r.volumes.is_empty() {
            let _ = out.try_send(pb::NodeMessage {
                msg: Some(pb::node_message::Msg::Disk(r)),
            });
        }
    }
}

pub async fn report(data_dir: &Path, runtime: &dyn Runtime) -> pb::DiskReport {
    let mut paths = vec![data_dir.to_path_buf()];
    if let Some(root) = runtime.data_root().await {
        paths.push(root);
    }
    let mut filesystems: Vec<(String, pb::FilesystemUsage)> = Vec::new();
    for p in paths {
        let Some((mount, fs)) = df(&p).await else {
            continue;
        };
        // Both on one filesystem: reported once.
        if !filesystems.iter().any(|(m, _)| *m == mount) {
            filesystems.push((mount, fs));
        }
    }
    let mut volumes = Vec::new();
    for (name, path) in runtime.volume_paths().await.unwrap_or_default() {
        if let Some(used_bytes) = du(&path).await {
            volumes.push(pb::VolumeUsage { name, used_bytes });
        }
    }
    pb::DiskReport {
        filesystems: filesystems.into_iter().map(|(_, f)| f).collect(),
        volumes,
    }
}

async fn df(path: &Path) -> Option<(String, pb::FilesystemUsage)> {
    let out = tokio::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let (mount, total, free) = parse_df(&String::from_utf8_lossy(&out.stdout))?;
    Some((
        mount,
        pb::FilesystemUsage {
            path: path.display().to_string(),
            total_bytes: total,
            free_bytes: free,
        },
    ))
}

async fn du(path: &Path) -> Option<u64> {
    let cmd = tokio::process::Command::new("du")
        .arg("-sk")
        .arg(path)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(DU_TIMEOUT, cmd).await.ok()?.ok()?;
    parse_du(&String::from_utf8_lossy(&out.stdout))
}

/// `df -Pk` → (mount point, total bytes, available bytes).
pub fn parse_df(out: &str) -> Option<(String, u64, u64)> {
    let line = out.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let total: u64 = cols[1].parse().ok()?;
    let avail: u64 = cols[3].parse().ok()?;
    Some((cols[5..].join(" "), total * 1024, avail * 1024))
}

/// `du -sk` → bytes (its first column is KiB).
pub fn parse_du(out: &str) -> Option<u64> {
    out.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|k| k * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_and_du_output() {
        let df = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                  /dev/sda1         81000000 40500000  36000000      53% /\n";
        assert_eq!(
            parse_df(df),
            Some(("/".into(), 81_000_000 * 1024, 36_000_000 * 1024))
        );
        assert_eq!(parse_df("Filesystem\n"), None);
        assert_eq!(
            parse_du("2048\t/var/lib/docker/volumes/x/_data\n"),
            Some(2048 * 1024)
        );
        assert_eq!(parse_du(""), None);
    }

    #[tokio::test]
    async fn reports_the_real_filesystem() {
        let rt = crate::runtime::fake::FakeRuntime::new();
        let r = report(&std::env::temp_dir(), &rt).await;
        assert_eq!(r.filesystems.len(), 1);
        assert!(r.filesystems[0].total_bytes > 0);
        assert!(r.volumes.is_empty());
    }
}
