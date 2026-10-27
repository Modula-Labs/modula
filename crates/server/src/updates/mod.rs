//! Keeping the instance up to date.
//!
//! * A daily check learns the newest release (signed `SHA256SUMS` + `VERSION`,
//!   see [`release`]) unless updates are off.
//! * `notify` shows it in the dashboard (and the audit log, once per version);
//!   `auto` also upgrades in the maintenance window ([`window`]).
//! * The upgrade itself is never run by this process (it runs as the
//!   unprivileged `modula` user). It writes the version to
//!   `<upgrade dir>/request`; the root-owned `modula-upgrade.path` unit that
//!   install.sh sets up runs `install.sh upgrade --version …` (signature
//!   checks, health check, automatic rollback) and writes `<dir>/result`,
//!   which [`reconcile`] records once this process (or its successor) reads it.
//! * After the control plane moved to a new version, [`rollout`] upgrades the
//!   other servers to it, one at a time.

pub mod release;
pub mod rollout;
pub mod window;

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// This control plane's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const TICK: Duration = Duration::from_secs(15);
const CHECK_EVERY: time::Duration = time::Duration::hours(24);
const RETRY_FAILED_CHECK: time::Duration = time::Duration::hours(1);
/// A request the upgrade unit never answered is given up after this.
const REQUEST_STALE: time::Duration = time::Duration::minutes(30);
/// How much of the installer's output is kept per upgrade.
const MAX_LOG: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Notify,
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Notify => "notify",
            Self::Auto => "auto",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "auto" => Self::Auto,
            _ => Self::Notify,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Settings {
    mode: String,
    pub channel: String,
    pub window_days: i16,
    pub window_minute: i16,
    /// IANA name; empty = the server's own time zone.
    pub timezone: String,
    pub latest_version: Option<String>,
    pub checked_at: Option<OffsetDateTime>,
    pub check_error: Option<String>,
    pub notified_version: Option<String>,
    pub rollout_cleared_at: Option<OffsetDateTime>,
}

impl Settings {
    pub fn mode(&self) -> Mode {
        Mode::parse(&self.mode)
    }

    pub fn window(&self) -> window::Window {
        window::Window {
            days: (self.window_days & 0x7f) as u8,
            minute: self.window_minute.clamp(0, 1439) as u16,
            tz: window::timezone(&self.timezone),
        }
    }

    /// The newest known release, when it is newer than this server.
    pub fn available(&self) -> Option<&str> {
        self.latest_version
            .as_deref()
            .filter(|v| release::is_newer(v, VERSION))
    }
}

pub async fn settings(db: &sqlx::PgPool) -> AppResult<Settings> {
    Ok(sqlx::query_as(
        "SELECT mode, channel, window_days, window_minute, timezone, latest_version, checked_at,
                check_error, notified_version, rollout_cleared_at
         FROM instance_updates",
    )
    .fetch_one(db)
    .await?)
}

fn system_meta() -> RequestMeta {
    RequestMeta {
        ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        user_agent: Some("modula-server/updates".into()),
    }
}

fn http() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .user_agent(concat!("modula-server/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}

/// Look for a new release now and record what was found.
pub async fn check(state: &AppState) -> AppResult<Settings> {
    let found = match http() {
        Ok(client) => {
            release::fetch_latest(&client, &state.cfg.releases.url, &state.cfg.releases.pubkey)
                .await
        }
        Err(e) => Err(e),
    };
    match found {
        Ok(latest) => {
            sqlx::query(
                "UPDATE instance_updates SET latest_version = coalesce($1, latest_version),
                        checked_at = now(), check_error = NULL",
            )
            .bind(latest.map(|v| v.to_string()))
            .execute(&state.db)
            .await?;
        }
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "update check failed");
            sqlx::query("UPDATE instance_updates SET checked_at = now(), check_error = $1")
                .bind(format!("{e:#}").chars().take(500).collect::<String>())
                .execute(&state.db)
                .await?;
        }
    }
    let s = settings(&state.db).await?;
    if let Some(v) = s.available()
        && s.notified_version.as_deref() != Some(v)
    {
        // There is no notification channel (email, webhooks) yet: the
        // dashboard shows it, and the audit log records it once per version.
        let mut tx = state.db.begin().await?;
        sqlx::query("UPDATE instance_updates SET notified_version = $1")
            .bind(v)
            .execute(&mut *tx)
            .await?;
        Entry::new("instance.update.available", &system_meta())
            .metadata(json!({ "current": VERSION, "latest": v }))
            .record(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(current = VERSION, latest = v, "a new release is available");
    }
    settings(&state.db).await
}

/// Who asked for an upgrade.
pub enum Trigger<'a> {
    Manual(&'a Actor, &'a RequestMeta),
    /// The maintenance window (auto mode).
    Auto,
}

/// Hand `version` to the upgrade unit. Recorded first: this process is
/// restarted halfway through, and its successor reads the result.
pub async fn request(state: &AppState, version: &str, trigger: Trigger<'_>) -> AppResult<Id> {
    let dir = upgrade_dir(state)?;
    if !release_version_ok(version) {
        return Err(AppError::bad_request("not a release version"));
    }
    let running: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM instance_upgrades WHERE status = 'requested' AND created_at > now() - $1::interval",
    )
    .bind(format!("{} seconds", REQUEST_STALE.whole_seconds()))
    .fetch_optional(&state.db)
    .await?;
    if running.is_some() {
        return Err(AppError::Conflict("an upgrade is already running".into()));
    }
    let id = Id::new();
    let system = system_meta();
    let (kind, user, meta) = match &trigger {
        Trigger::Manual(actor, meta) => ("manual", Some(actor.user_id), *meta),
        Trigger::Auto => ("auto", None, &system),
    };
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO instance_upgrades (id, from_version, to_version, trigger, requested_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(VERSION)
    .bind(version)
    .bind(kind)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    let mut entry = Entry::new("instance.upgrade.request", meta)
        .target("instance_upgrade", id)
        .metadata(json!({ "from": VERSION, "to": version, "trigger": kind }));
    if let Trigger::Manual(actor, _) = trigger {
        entry = entry.actor(actor);
    }
    entry.record(&mut *tx).await?;
    tx.commit().await?;

    if let Err(e) = write_request(&dir, version).await {
        sqlx::query(
            "UPDATE instance_upgrades SET status = 'failed', log = $2, finished_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(format!("could not hand the upgrade to modula-upgrade: {e:#}"))
        .execute(&state.db)
        .await?;
        return Err(AppError::Internal(e));
    }
    tracing::info!(
        from = VERSION,
        to = version,
        trigger = kind,
        "upgrade requested"
    );
    Ok(id)
}

/// Versions the installer accepts (the node and install.sh check the same).
fn release_version_ok(v: &str) -> bool {
    semver::Version::parse(v).is_ok() && v.len() <= 64
}

fn upgrade_dir(state: &AppState) -> AppResult<std::path::PathBuf> {
    state
        .cfg
        .releases
        .upgrade_dir
        .clone()
        .filter(|d| d.is_dir())
        .ok_or_else(|| {
            AppError::Conflict(
                "this server can't upgrade itself from the dashboard; run install.sh upgrade (it sets this up)"
                    .into(),
            )
        })
}

async fn write_request(dir: &Path, version: &str) -> anyhow::Result<()> {
    // Renamed into place, so the path unit only ever sees a complete file.
    let tmp = dir.join(".request.tmp");
    tokio::fs::write(&tmp, format!("{version}\n")).await?;
    tokio::fs::rename(&tmp, dir.join("request")).await?;
    Ok(())
}

/// The upgrade unit's report (`<dir>/result`): `key=value` lines, a blank
/// line, then the tail of the installer's output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpgradeResult {
    /// `succeeded`, `failed` or `rolled_back`.
    pub status: String,
    pub to: String,
    pub log: String,
}

impl UpgradeResult {
    pub fn parse(text: &str) -> Self {
        let (head, log) = text.split_once("\n\n").unwrap_or((text, ""));
        let mut r = Self {
            log: log.trim().to_owned(),
            ..Self::default()
        };
        for line in head.lines() {
            match line.split_once('=') {
                Some(("status", v)) => r.status = v.trim().to_owned(),
                Some(("to", v)) => r.to = v.trim().to_owned(),
                _ => {}
            }
        }
        r
    }
}

/// Record the upgrade unit's report for the request it answers, and give up
/// on requests it never answered.
pub async fn reconcile(state: &AppState) -> anyhow::Result<()> {
    let Some(dir) = state.cfg.releases.upgrade_dir.as_ref() else {
        return Ok(());
    };
    if let Ok(text) = tokio::fs::read_to_string(dir.join("result")).await {
        let r = UpgradeResult::parse(&text);
        let status = match r.status.as_str() {
            s @ ("succeeded" | "failed" | "rolled_back") => s,
            _ => "failed",
        };
        let mut log = r.log;
        if log.len() > MAX_LOG {
            let mut start = log.len() - MAX_LOG;
            while !log.is_char_boundary(start) {
                start += 1;
            }
            log = log[start..].to_owned();
        }
        let mut tx = state.db.begin().await?;
        let id: Option<Id> = sqlx::query_scalar(
            "UPDATE instance_upgrades SET status = $2, log = $3, finished_at = now()
             WHERE id = (SELECT id FROM instance_upgrades WHERE status = 'requested' AND to_version = $1
                         ORDER BY created_at DESC LIMIT 1)
             RETURNING id",
        )
        .bind(&r.to)
        .bind(status)
        .bind(&log)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = id {
            let action = match status {
                "succeeded" => "instance.upgrade.succeeded",
                "rolled_back" => "instance.upgrade.rolled_back",
                _ => "instance.upgrade.failed",
            };
            Entry::new(action, &system_meta())
                .target("instance_upgrade", id)
                .metadata(json!({ "to": r.to, "running": VERSION }))
                .record(&mut *tx)
                .await?;
            tracing::info!(to = %r.to, status, "upgrade finished");
        }
        tx.commit().await?;
    }
    sqlx::query(
        "UPDATE instance_upgrades SET status = 'failed', finished_at = now(),
                log = 'The upgrade service did not report back. Is modula-upgrade.path installed? (journalctl -u modula-upgrade)'
         WHERE status = 'requested' AND created_at < now() - $1::interval",
    )
    .bind(format!("{} seconds", REQUEST_STALE.whole_seconds()))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Background: results, daily checks, and upgrades in the maintenance window.
pub async fn run(state: AppState, stop: CancellationToken) {
    loop {
        if let Err(e) = tick(&state).await {
            tracing::warn!(error = format!("{e:#}"), "updates");
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(TICK) => {}
        }
    }
}

/// One pass of [`run`] (tests drive it directly).
pub async fn tick(state: &AppState) -> anyhow::Result<()> {
    // Never upgrade while the control plane moves (migrate::maintenance).
    if state.maintenance.is_on() {
        return Ok(());
    }
    reconcile(state).await?;
    let mut s = settings(&state.db)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if s.mode() == Mode::Off {
        return Ok(());
    }
    let due = s.checked_at.is_none_or(|at| {
        let every = if s.check_error.is_some() {
            RETRY_FAILED_CHECK
        } else {
            CHECK_EVERY
        };
        at + every < OffsetDateTime::now_utc()
    });
    if due {
        s = check(state).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let Some(latest) = s.available().map(str::to_owned) else {
        return Ok(());
    };
    let can_upgrade = state
        .cfg
        .releases
        .upgrade_dir
        .as_ref()
        .is_some_and(|d| d.is_dir());
    if s.mode() != Mode::Auto || !can_upgrade {
        return Ok(());
    }
    let now = jiff::Timestamp::now();
    if !s.window().contains(now) {
        return Ok(());
    }
    // Each version is tried automatically once; a failed one waits for "Update now".
    let tried: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM instance_upgrades WHERE to_version = $1)")
            .bind(&latest)
            .fetch_one(&state.db)
            .await?;
    if !tried {
        request(state, &latest, Trigger::Auto)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("automatic upgrade")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_upgrade_report() {
        let r = UpgradeResult::parse(
            "status=succeeded\nfrom=0.1.0\nto=0.2.0\nfinished_at=2026-10-25T03:01:00Z\n\n  ✓ Upgraded to 0.2.0\n",
        );
        assert_eq!(r.status, "succeeded");
        assert_eq!(r.to, "0.2.0");
        assert_eq!(r.log, "✓ Upgraded to 0.2.0");
    }

    #[test]
    fn versions_for_the_installer() {
        assert!(release_version_ok("0.2.0"));
        assert!(!release_version_ok("latest"));
        assert!(!release_version_ok("0.2.0; reboot"));
    }
}
