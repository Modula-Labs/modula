//! Becoming the main server (migrating the control plane here). On
//! `PromoteRequest` the node fetches the promotion bundle from the control
//! plane it is connected to (`FetchPromotion`, with its own client
//! certificate and the request's single-use ticket) into
//! `<data dir>/promote/bundle.tar`, then the installer sets the control plane
//! up from it: `install.sh main --restore <bundle>` from this server's own
//! (verified) release, run by systemd outside this service's cgroup.
//!
//! The installer neither restarts modula-node nor changes `node.json`: this
//! node stays connected to the old control plane, reports the outcome there
//! (`PromoteStatus`), and switches over when told (`ServerAddress`, see
//! crate::address), restarting to serve the dashboard.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, bail};
use async_trait::async_trait;
use modula_proto::node::v1 as pb;
use pb::node_client::NodeClient;
use pb::node_message::Msg as Out;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::identity::Identity;

const DIR: &str = "promote";
const BUNDLE: &str = "bundle.tar";
const LOG: &str = "install.log";
/// How much of the installer's output a failure carries.
const ERROR_TAIL: usize = 2000;

#[async_trait]
pub trait Promoter: Send + Sync {
    /// Set the control plane up from `bundle`; Err carries why it failed.
    async fn install(&self, bundle: &Path) -> anyhow::Result<()>;
}

static PROMOTER: OnceLock<Arc<dyn Promoter>> = OnceLock::new();
static BUSY: AtomicBool = AtomicBool::new(false);

/// Registered once by `modula-node run` (or a test); without one the node
/// doesn't offer the `promote` capability.
pub fn register(p: Arc<dyn Promoter>) {
    let _ = PROMOTER.set(p);
}

pub fn promoter() -> Option<Arc<dyn Promoter>> {
    PROMOTER.get().cloned()
}

/// Runs the release's installer with `systemd-run --wait`.
pub struct SystemdPromoter {
    installer: PathBuf,
    log: PathBuf,
}

impl SystemdPromoter {
    /// None without the installer in the current release (development).
    pub fn detect(prefix: &Path, data_dir: &Path) -> Option<Self> {
        let installer = prefix.join("current/install.sh");
        let systemd_run = ["/usr/bin/systemd-run", "/bin/systemd-run"]
            .iter()
            .any(|p| Path::new(p).exists());
        (installer.is_file() && systemd_run).then(|| Self {
            installer,
            log: data_dir.join(DIR).join(LOG),
        })
    }
}

#[async_trait]
impl Promoter for SystemdPromoter {
    async fn install(&self, bundle: &Path) -> anyhow::Result<()> {
        let _ = tokio::fs::remove_file(&self.log).await;
        let unit = format!("modula-promote-{}", &modula_id::Id::new().to_string()[..8]);
        let log = self.log.display().to_string();
        let status = tokio::process::Command::new("systemd-run")
            .args([
                &format!("--unit={unit}"),
                "--collect",
                "--wait",
                "--quiet",
                &format!("--property=StandardOutput=append:{log}"),
                &format!("--property=StandardError=append:{log}"),
                "--property=TimeoutStartSec=40min",
                "--setenv=NO_COLOR=1",
                "/bin/bash",
            ])
            .arg(&self.installer)
            .args(["main", "--restore"])
            .arg(bundle)
            .status()
            .await
            .context("cannot run systemd-run")?;
        if status.success() {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&self.log)
            .await
            .unwrap_or_default();
        bail!(
            "the installer failed ({status}): {}",
            tail(text.trim(), ERROR_TAIL)
        )
    }
}

fn tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// A stand-in for tests: keeps every bundle it was given.
#[derive(Default)]
pub struct FakePromoter {
    pub bundles: Mutex<Vec<Vec<u8>>>,
    pub fail: Mutex<Option<String>>,
}

#[async_trait]
impl Promoter for FakePromoter {
    async fn install(&self, bundle: &Path) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(bundle).await?;
        self.bundles.lock().expect("fake promoter").push(bytes);
        if let Some(e) = self.fail.lock().expect("fake promoter").clone() {
            bail!("{e}");
        }
        Ok(())
    }
}

/// Download the bundle `req` names into `dest` (mode 0600), checking its
/// size and SHA-256 against the last chunk. Returns its size.
pub async fn fetch(id: &Identity, req: &pb::PromoteRequest, dest: &Path) -> anyhow::Result<u64> {
    let channel = crate::join::connect(&id.meta.server, id.fingerprint()?, Some(id)).await?;
    let mut stream = NodeClient::new(channel)
        .fetch_promotion(pb::PromotionFetch {
            request_id: req.request_id.clone(),
            ticket: req.ticket.clone(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("the control plane refused the bundle: {}", s.message()))?
        .into_inner();
    if let Some(dir) = dest.parent() {
        create_private_dir(dir)?;
    }
    let mut file = {
        let mut o = tokio::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        o.mode(0o600);
        o.open(dest)
            .await
            .with_context(|| format!("cannot write {}", dest.display()))?
    };
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut expected = None;
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow::anyhow!("the bundle download broke off: {}", s.message()))?
    {
        hash.update(&chunk.data);
        total += chunk.data.len() as u64;
        file.write_all(&chunk.data).await?;
        if !chunk.sha256.is_empty() {
            expected = Some((chunk.sha256, chunk.total_bytes));
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    let Some((sum, size)) = expected else {
        bail!("the bundle download ended early");
    };
    if size != total || hex::encode(hash.finalize()) != sum {
        bail!("the bundle arrived damaged ({total} of {size} bytes)");
    }
    Ok(total)
}

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn status(request_id: &str, done: bool, result: Result<(), String>, step: &str) -> pb::NodeMessage {
    let (ok, error) = match result {
        Ok(()) => (done, String::new()),
        Err(e) => (false, e),
    };
    pb::NodeMessage {
        msg: Some(Out::PromoteStatus(pb::PromoteStatus {
            request_id: request_id.to_owned(),
            done,
            ok,
            error,
            step: step.to_owned(),
        })),
    }
}

/// Handle one `PromoteRequest`: fetch, install, report (through `results`,
/// which outlives reconnects).
pub async fn run(dir: PathBuf, req: pb::PromoteRequest, results: mpsc::Sender<pb::NodeMessage>) {
    let rid = req.request_id.clone();
    let Some(promoter) = promoter() else {
        let why =
            "this server can't become the main server (modula-node runs without the installer)";
        let _ = results.send(status(&rid, true, Err(why.into()), "")).await;
        return;
    };
    if BUSY.swap(true, Ordering::SeqCst) {
        tracing::warn!("ignoring a promotion request: one is already running");
        return;
    }
    let outcome = async {
        let id = Identity::load(&dir)?;
        let bundle = dir.join(DIR).join(BUNDLE);
        let _ = results
            .send(status(&rid, false, Ok(()), "Downloading the control plane"))
            .await;
        let size = fetch(&id, &req, &bundle).await?;
        tracing::info!(bytes = size, "promotion bundle downloaded");
        let _ = results
            .send(status(
                &rid,
                false,
                Ok(()),
                &format!(
                    "Installing the control plane ({} MB copied)",
                    size.div_ceil(1_000_000)
                ),
            ))
            .await;
        promoter.install(&bundle).await
    }
    .await;
    BUSY.store(false, Ordering::SeqCst);
    let result = outcome.map_err(|e: anyhow::Error| {
        tracing::warn!(error = format!("{e:#}"), "promotion failed");
        format!("{e:#}")
    });
    let _ = results.send(status(&rid, true, result, "")).await;
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn tails_on_char_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("héllo", 4), "llo");
    }
}
