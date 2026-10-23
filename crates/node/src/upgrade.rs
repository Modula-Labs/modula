//! Self-upgrade on the control plane's request (rolling upgrades).
//!
//! The node never replaces itself: it writes the requested version to
//! `<dir>/request`, which the root-owned `modula-upgrade.path` unit (installed
//! by install.sh) watches. Its service runs the installer's `upgrade` from this
//! instance: it downloads the release, checks its signature, switches
//! `/opt/modula/current` and restarts modula-node (and Caddy only if Caddy
//! changed). It runs outside this service's cgroup, so restarting modula-node
//! doesn't kill it halfway. The outcome lands in `<dir>/result`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use async_trait::async_trait;

/// How long an upgrade (download + switch + restart) may take.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How much of the installer's output a failure carries.
const ERROR_TAIL: usize = 2000;

#[async_trait]
pub trait Upgrader: Send + Sync {
    /// The version this node reports in Hello.
    fn version(&self) -> String;
    /// Install `version`. On success the process is normally restarted before
    /// this returns; `Ok` means the new version is in place and the session
    /// should reconnect. `Err` carries why it failed (the installer rolled back).
    async fn upgrade(&self, version: &str) -> anyhow::Result<()>;
}

/// Release versions as the installer accepts them (`--version X.Y.Z`).
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.as_bytes()[0].is_ascii_digit()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Hands upgrades to the `modula-upgrade` systemd unit.
pub struct SystemdUpgrader {
    dir: PathBuf,
    busy: AtomicBool,
}

impl SystemdUpgrader {
    /// `None` when the installer didn't set up the upgrade unit here (older
    /// installs, development): the node then doesn't offer upgrades.
    pub fn detect(dir: &Path) -> Option<Self> {
        dir.is_dir().then(|| Self {
            dir: dir.to_owned(),
            busy: AtomicBool::new(false),
        })
    }

    fn result_modified(&self) -> Option<SystemTime> {
        std::fs::metadata(self.dir.join("result"))
            .and_then(|m| m.modified())
            .ok()
    }
}

#[async_trait]
impl Upgrader for SystemdUpgrader {
    fn version(&self) -> String {
        crate::VERSION.to_owned()
    }

    async fn upgrade(&self, version: &str) -> anyhow::Result<()> {
        if !valid_version(version) {
            bail!("not a release version: {version:?}");
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            bail!("an upgrade is already running on this server");
        }
        let outcome = self.run(version).await;
        self.busy.store(false, Ordering::SeqCst);
        outcome
    }
}

impl SystemdUpgrader {
    async fn run(&self, version: &str) -> anyhow::Result<()> {
        let before = self.result_modified();
        // Renamed into place: the path unit fires on a complete file only.
        let tmp = self.dir.join(".request.tmp");
        tokio::fs::write(&tmp, format!("{version}\n"))
            .await
            .context("cannot write the upgrade request")?;
        tokio::fs::rename(&tmp, self.dir.join("request"))
            .await
            .context("cannot write the upgrade request")?;
        tracing::info!(%version, "upgrade requested from modula-upgrade");

        let started = Instant::now();
        while started.elapsed() < UPGRADE_TIMEOUT {
            tokio::time::sleep(POLL_EVERY).await;
            if self.result_modified() == before {
                continue;
            }
            let text = tokio::fs::read_to_string(self.dir.join("result"))
                .await
                .unwrap_or_default();
            let result = UpgradeResult::parse(&text);
            if result.to != version {
                continue; // an older run's result
            }
            return match result.status.as_str() {
                "succeeded" => Ok(()),
                status => bail!("upgrade {status}: {}", result.log_tail(ERROR_TAIL)),
            };
        }
        bail!("the upgrade did not finish within 15 minutes")
    }
}

/// The installer's report (`<dir>/result`): `key=value` lines, a blank line,
/// then the tail of its output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpgradeResult {
    /// `succeeded`, `failed` or `rolled_back`.
    pub status: String,
    pub from: String,
    pub to: String,
    pub finished_at: String,
    pub log: String,
}

impl UpgradeResult {
    pub fn parse(text: &str) -> Self {
        let (head, log) = text.split_once("\n\n").unwrap_or((text, ""));
        let mut r = Self {
            log: log.to_owned(),
            ..Self::default()
        };
        for line in head.lines() {
            match line.split_once('=') {
                Some(("status", v)) => r.status = v.trim().to_owned(),
                Some(("from", v)) => r.from = v.trim().to_owned(),
                Some(("to", v)) => r.to = v.trim().to_owned(),
                Some(("finished_at", v)) => r.finished_at = v.trim().to_owned(),
                _ => {}
            }
        }
        r
    }

    /// The last `max` bytes of the log, on a character boundary.
    pub fn log_tail(&self, max: usize) -> &str {
        let log = self.log.trim();
        let mut start = log.len().saturating_sub(max);
        while !log.is_char_boundary(start) {
            start += 1;
        }
        log[start..].trim_start()
    }
}

/// A stand-in for tests: "installs" a version by reporting it from then on.
#[derive(Default)]
pub struct FakeUpgrader {
    pub current: Mutex<String>,
    /// Every requested version, in order, with when it was asked for.
    pub requests: Mutex<Vec<(String, Instant)>>,
    /// Fail every upgrade with this message.
    pub fail: Mutex<Option<String>>,
}

impl FakeUpgrader {
    pub fn new(version: &str) -> Self {
        Self {
            current: Mutex::new(version.to_owned()),
            ..Self::default()
        }
    }
}

#[async_trait]
impl Upgrader for FakeUpgrader {
    fn version(&self) -> String {
        self.current.lock().expect("fake upgrader").clone()
    }

    async fn upgrade(&self, version: &str) -> anyhow::Result<()> {
        self.requests
            .lock()
            .expect("fake upgrader")
            .push((version.to_owned(), Instant::now()));
        if let Some(e) = self.fail.lock().expect("fake upgrader").clone() {
            bail!("{e}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        *self.current.lock().expect("fake upgrader") = version.to_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        for ok in ["0.1.0", "1.2.3-rc.1", "2.0.0+build.5"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in ["", "latest", "v1.2.3", "1.2.3; rm -rf /", "1.2.3\n", "../1"] {
            assert!(!valid_version(bad), "{bad:?}");
        }
    }

    #[test]
    fn parses_the_installers_report() {
        let r = UpgradeResult::parse(
            "status=rolled_back\nfrom=0.1.0\nto=0.2.0\nfinished_at=2026-10-23T03:00:12Z\n\nline 1\nmodula-server did not become healthy\n",
        );
        assert_eq!(r.status, "rolled_back");
        assert_eq!((r.from.as_str(), r.to.as_str()), ("0.1.0", "0.2.0"));
        assert_eq!(r.finished_at, "2026-10-23T03:00:12Z");
        assert_eq!(r.log_tail(37), "modula-server did not become healthy");
        assert_eq!(UpgradeResult::parse("garbage").status, "");
    }

    #[tokio::test]
    async fn hands_the_version_to_the_unit_and_waits_for_its_result() {
        let dir = std::env::temp_dir().join(format!("modula-upgrade-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let up = std::sync::Arc::new(SystemdUpgrader::detect(&dir).unwrap());
        assert!(up.upgrade("not a version").await.is_err());

        let task = tokio::spawn({
            let up = up.clone();
            async move { up.upgrade("0.2.0").await }
        });
        // The unit: picks up the request, then reports a rollback.
        let request = dir.join("request");
        for _ in 0..50 {
            if request.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(std::fs::read_to_string(&request).unwrap(), "0.2.0\n");
        assert!(up.upgrade("0.2.0").await.is_err(), "one at a time");
        std::fs::remove_file(&request).unwrap();
        std::fs::write(
            dir.join("result"),
            "status=rolled_back\nfrom=0.1.0\nto=0.2.0\n\nhealth check failed\n",
        )
        .unwrap();
        let err = task.await.unwrap().unwrap_err().to_string();
        assert!(
            err.contains("rolled_back") && err.contains("health check failed"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
