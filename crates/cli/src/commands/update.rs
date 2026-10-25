//! `update` (this CLI, from the signed release) and `server update` (the
//! logged-in instance, through its Updates API, like the dashboard's
//! Settings → Updates).

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{Api, api_error};
use crate::context::Ctx;
use crate::output::{Output, Table};
use crate::update::{self, VERSION};

// ---------------------------------------------------------------- the CLI

/// `modula update [--check]`.
pub async fn cli(ctx: &Ctx, check: bool) -> anyhow::Result<i32> {
    let base = update::release_base();
    let key = update::release_pubkey()?;
    let http = update::http_client(Duration::from_secs(30))?;
    if !ctx.out.json {
        ctx.out.note(ctx.out.dim("Checking for a new modula…"));
    }
    let release = update::fetch_latest(&http, &base, &key)
        .await
        .context("cannot check for a new modula")?;
    let latest = release.version.to_string();
    let newer = update::is_newer(&latest, VERSION);
    remember_cli(ctx, &latest);
    let notes = update::release_page(&base, &latest);
    if !newer || check {
        ctx.out.either(
            &json!({
                "current": VERSION,
                "latest": latest,
                "updateAvailable": newer,
                "releaseNotesUrl": newer.then_some(&notes),
            }),
            |o| {
                if newer {
                    o.line(format!(
                        "modula {VERSION} → {}  (run `modula update`)",
                        o.accent(&latest)
                    ));
                    o.line(format!("Release notes: {notes}"));
                } else {
                    o.line(format!("modula {VERSION} is up to date"));
                }
            },
        )?;
        return Ok(0);
    }

    let asset = update::this_asset().with_context(|| {
        format!(
            "there is no prebuilt modula for {}/{}; build it from source: cargo install --path crates/cli",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let sha = release
        .sha256_of(&asset)
        .with_context(|| format!("release {latest} has no {asset} binary (yet)"))?
        .to_owned();
    let exe = update::current_exe()?;
    if !ctx.out.json {
        ctx.out
            .line(format!("modula {VERSION} → {}", ctx.out.accent(&latest)));
        ctx.out.line(format!("Release notes: {notes}"));
    }
    let path = exe.to_string_lossy();
    if path.contains("/target/debug/") || path.contains("/target/release/") {
        ctx.confirm(&format!(
            "{} is a build from source; replace it with the {latest} release?",
            exe.display()
        ))?;
    }
    check_writable(&exe)?;

    if !ctx.out.json {
        ctx.out.note(ctx.out.dim(&format!("Downloading {asset}…")));
    }
    let big = update::http_client(Duration::from_secs(600))?;
    let bytes = update::download(&big, &base, &sha).await?;
    update::replace_file(&exe, &bytes)?;
    let installed = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    ctx.out.either(
        &json!({
            "current": latest,
            "previous": VERSION,
            "updated": true,
            "path": exe,
        }),
        |o| {
            o.line(format!(
                "✓ Updated to {} ({}, checksum and signature verified)",
                if installed.is_empty() {
                    format!("modula {latest}")
                } else {
                    installed.clone()
                },
                exe.display()
            ));
        },
    )?;
    Ok(0)
}

/// Fail before downloading when the executable's directory isn't writable.
fn check_writable(exe: &std::path::Path) -> anyhow::Result<()> {
    let dir = exe.parent().context("the executable has no directory")?;
    let probe = dir.join(format!(".modula-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "no permission to replace {} (in {}). Run `sudo modula update`, or reinstall: \
             curl -fsSL https://usemodula.org/install-cli.sh | bash",
            exe.display(),
            dir.display()
        ),
        Err(e) => Err(e).with_context(|| format!("cannot write to {}", dir.display())),
    }
}

/// Record the newest CLI release, so the daily notice stays quiet today.
fn remember_cli(ctx: &Ctx, latest: &str) {
    let path = update::cache_path(&ctx.session.config_path);
    let mut cache = update::load_cache(&path);
    cache.cli_latest = Some(latest.to_owned());
    cache.cli_checked_at = Some(update::now());
    update::save_cache(&path, &cache);
}

// ---------------------------------------------------------------- the server

const STATUS: &str = "/api/instance/updates";
const POLL: Duration = Duration::from_secs(3);
/// How long the instance may be unreachable (restarting into the new version).
const UNREACHABLE_FOR: Duration = Duration::from_secs(5 * 60);
/// How long an upgrade, or the dashboard click that starts it, is waited for.
const FOLLOW_FOR: Duration = Duration::from_secs(40 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_notes_url: Option<String>,
    pub checked_at: Option<String>,
    pub check_error: Option<String>,
    pub mode: String,
    #[serde(default)]
    pub can_upgrade: bool,
    pub last_run: Option<Run>,
    #[serde(default)]
    pub servers: Vec<ServerVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    pub trigger: String,
    /// `requested`, `running`, `succeeded`, `failed` or `rolled_back`.
    pub status: String,
    pub log: Option<String>,
    pub requested_at: String,
    pub finished_at: Option<String>,
}

impl Run {
    fn active(&self) -> bool {
        matches!(self.status.as_str(), "requested" | "running")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerVersion {
    pub node_id: String,
    pub name: String,
    pub version: Option<String>,
    /// `current`, `behind`, `upgrading`, `failed`, `manual` or `offline`.
    pub status: String,
    pub error: Option<String>,
}

impl UpdateStatus {
    fn failed_servers(&self) -> usize {
        self.servers.iter().filter(|s| s.status == "failed").count()
    }

    fn rolling_out(&self) -> bool {
        self.servers
            .iter()
            .any(|s| matches!(s.status.as_str(), "behind" | "upgrading"))
    }
}

/// What `server update` does with a status.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// An upgrade is already under way: follow it.
    Follow,
    /// A newer release: upgrade to it.
    Upgrade(String),
    /// Current, but servers failed to follow: retry them.
    RetryServers,
    UpToDate,
}

pub fn next(s: &UpdateStatus) -> Next {
    if s.last_run.as_ref().is_some_and(Run::active) {
        return Next::Follow;
    }
    match s.latest_version.as_deref() {
        Some(v) if s.update_available => Next::Upgrade(v.to_owned()),
        _ if s.failed_servers() > 0 => Next::RetryServers,
        _ => Next::UpToDate,
    }
}

/// `modula server update [--check]`.
pub async fn server(ctx: &Ctx, check: bool) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let instance = api.login.url.clone();
    // Ask the instance to check now (what the dashboard's "Check now" does);
    // fall back to what it last knew.
    let status = match api
        .post_empty::<UpdateStatus>(&format!("{STATUS}/check"))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            explain_access(&e)?;
            let s: UpdateStatus = match api.get(STATUS).await {
                Ok(s) => s,
                Err(e) => {
                    explain_access(&e)?;
                    return Err(e);
                }
            };
            ctx.out.note(format!(
                "note: the instance couldn't check for a new release: {e:#}"
            ));
            s
        }
    };
    remember_server(ctx, instance.as_str(), &status);
    let next = next(&status);
    if check || ctx.out.json && next == Next::UpToDate {
        ctx.out
            .either(&status, |o| print_status(o, instance.as_str(), &status))?;
        return Ok(0);
    }
    if !ctx.out.json {
        print_status(&ctx.out, instance.as_str(), &status);
    }

    let before = status.last_run.as_ref().map(|r| r.id.clone());
    let wait = match next {
        Next::UpToDate => return Ok(0),
        Next::Follow => Wait::Run(before.clone().unwrap_or_default()),
        Next::Upgrade(ref to) => {
            if !status.can_upgrade {
                bail!(
                    "{instance} can't upgrade itself (it was installed without the upgrade \
                     units). On the server, run:\n  curl -fsSL https://usemodula.org/install.sh \
                     | sudo bash -s -- upgrade --version {to}"
                );
            }
            ctx.confirm(&format!(
                "Update {instance} from {} to {to}?",
                status.current_version
            ))?;
            match start(ctx, api, &status).await? {
                Some(id) => Wait::Run(id),
                None => Wait::NewRun(before),
            }
        }
        Next::RetryServers => {
            ctx.confirm(&format!(
                "Retry upgrading {} server(s) to {}?",
                status.failed_servers(),
                status.current_version
            ))?;
            start(ctx, api, &status).await?;
            Wait::Servers
        }
    };
    follow(ctx, api, wait).await
}

/// "Update now". `Ok(Some(run id))` when it started an upgrade of the
/// control plane, `Ok(None)` when it waits for one started in the dashboard
/// (or retried the servers).
async fn start(ctx: &Ctx, api: &Api, before: &UpdateStatus) -> anyhow::Result<Option<String>> {
    match api
        .post_empty::<UpdateStatus>(&format!("{STATUS}/run"))
        .await
    {
        Ok(s) => Ok(s
            .last_run
            .filter(|r| Some(&r.id) != before.last_run.as_ref().map(|b| &b.id))
            .map(|r| r.id)),
        Err(e) if api_error(&e).is_some_and(|a| a.code == "sudo_required") => {
            let url = api.login.url.join("settings#updates")?;
            if !ctx.session.interactive || ctx.out.json || ctx.out.is_captured() {
                bail!(
                    "starting an update needs a recent sign-in, which the CLI's API tokens \
                     never have: click Update now in the dashboard ({url}), then run \
                     `modula server update` to follow it"
                );
            }
            ctx.out.note(format!(
                "Starting an update needs a recent sign-in, which the CLI's API tokens never \
                 have.\nOpening the dashboard: click {} there and this command follows it.\n  {url}",
                ctx.out.accent("Update now")
            ));
            crate::commands::project::open_in_browser(url.as_str());
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// What [`follow`] waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wait {
    /// This control-plane upgrade, then the servers following it.
    Run(String),
    /// An upgrade started in the dashboard: the first run after this one.
    NewRun(Option<String>),
    /// Servers being retried (no control-plane upgrade).
    Servers,
}

/// Poll until the upgrade finishes, then until the other servers follow.
async fn follow(ctx: &Ctx, api: &Api, mut wait: Wait) -> anyhow::Result<i32> {
    let o = &ctx.out;
    let started = Instant::now();
    let mut unreachable_since: Option<Instant> = None;
    let mut shown_run: Option<String> = None;
    let mut shown_servers: Vec<(String, String)> = Vec::new();
    let mut seen_rolling = false;
    if !o.json {
        o.note(o.dim("Following the update (Ctrl-C stops watching; the update continues)…"));
    }
    loop {
        if started.elapsed() > FOLLOW_FOR {
            bail!(
                "gave up waiting after {} minutes; run `modula server update --check` to see where it is",
                FOLLOW_FOR.as_secs() / 60
            );
        }
        let s: UpdateStatus = match api.get(STATUS).await {
            Ok(s) => {
                if unreachable_since.take().is_some() && !o.json {
                    o.note(o.dim("The instance is back."));
                }
                s
            }
            Err(e) => {
                if api_error(&e).is_some() {
                    return Err(e);
                }
                let since = *unreachable_since.get_or_insert_with(|| {
                    if !o.json {
                        o.note(
                            o.dim("Waiting for the instance (it restarts into the new version)…"),
                        );
                    }
                    Instant::now()
                });
                if since.elapsed() > UNREACHABLE_FOR {
                    return Err(e).context("the instance didn't come back");
                }
                tokio::time::sleep(POLL).await;
                continue;
            }
        };

        // An upgrade started in the dashboard.
        if let Wait::NewRun(before) = &wait
            && let Some(r) = &s.last_run
            && Some(&r.id) != before.as_ref()
        {
            wait = Wait::Run(r.id.clone());
        }
        let control_plane_done = match &wait {
            Wait::NewRun(_) => false,
            Wait::Servers => true,
            Wait::Run(id) => match s.last_run.as_ref().filter(|r| &r.id == id) {
                // Not listed (yet): keep waiting.
                None => false,
                Some(r) => {
                    if shown_run.as_deref() != Some(r.status.as_str()) {
                        shown_run = Some(r.status.clone());
                        if !o.json {
                            o.line(format!(
                                "  Control plane {} → {}: {}",
                                r.from_version,
                                r.to_version,
                                o.status(&r.status.replace('_', " "))
                            ));
                        }
                    }
                    match r.status.as_str() {
                        "succeeded" => true,
                        "failed" | "rolled_back" => {
                            if o.json {
                                o.json(&s)?;
                            } else {
                                let log = r.log.as_deref().unwrap_or_default();
                                let tail: Vec<&str> = log.lines().rev().take(20).collect();
                                for l in tail.into_iter().rev() {
                                    o.note(format!("    {}", o.dim(l)));
                                }
                                o.note(format!(
                                    "The upgrade {}; the instance still runs {}.",
                                    r.status.replace('_', " "),
                                    s.current_version
                                ));
                            }
                            return Ok(1);
                        }
                        _ => false,
                    }
                }
            },
        };
        if !control_plane_done {
            tokio::time::sleep(POLL).await;
            continue;
        }

        for sv in &s.servers {
            let key = (sv.node_id.clone(), sv.status.clone());
            if !shown_servers.contains(&key) {
                shown_servers.retain(|(id, _)| id != &sv.node_id);
                shown_servers.push(key);
                if !o.json && s.servers.len() > 1 {
                    o.line(format!(
                        "  Server {}: {}{}",
                        sv.name,
                        o.status(&sv.status),
                        sv.error
                            .as_deref()
                            .map(|e| format!(" ({e})"))
                            .unwrap_or_default()
                    ));
                }
            }
        }
        seen_rolling |= s.rolling_out();
        let failed = s.failed_servers();
        // A retry started in the dashboard shows up as servers rolling out.
        let waiting_for_retry = wait == Wait::Servers && !seen_rolling && failed > 0;
        if !s.rolling_out() && !waiting_for_retry {
            remember_server(ctx, api.login.url.as_str(), &s);
            o.either(&s, |o| {
                o.line(format!(
                    "✓ {} runs Modula Deploy {}",
                    api.login.url,
                    o.accent(&s.current_version)
                ));
                if failed > 0 {
                    o.line(format!(
                        "  {failed} server(s) failed to upgrade; see Settings → Updates, or run `modula server update` to retry"
                    ));
                }
            })?;
            return Ok(if failed > 0 { 1 } else { 0 });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Clear messages for a non-admin or a server too old for this.
fn explain_access(e: &anyhow::Error) -> anyhow::Result<()> {
    match api_error(e) {
        Some(a) if a.code == "forbidden" => {
            bail!("updating the instance needs an instance admin (this login isn't one)")
        }
        Some(a) if a.status == 404 => bail!(
            "this instance has no Updates API (it predates it); upgrade it on the server: \
             curl -fsSL https://usemodula.org/install.sh | sudo bash -s -- upgrade"
        ),
        _ => Ok(()),
    }
}

fn remember_server(ctx: &Ctx, url: &str, s: &UpdateStatus) {
    let path = update::cache_path(&ctx.session.config_path);
    let mut cache = update::load_cache(&path);
    cache.servers.insert(
        url.to_owned(),
        update::ServerCheck {
            checked_at: update::now(),
            available: s.latest_version.clone().filter(|_| s.update_available),
            current: Some(s.current_version.clone()),
        },
    );
    update::save_cache(&path, &cache);
}

fn print_status(o: &Output, instance: &str, s: &UpdateStatus) {
    o.line(format!("Instance   {instance}"));
    match (&s.latest_version, s.update_available) {
        (Some(v), true) => o.line(format!(
            "Version    {} → {}  (update available)",
            s.current_version,
            o.accent(v)
        )),
        _ => o.line(format!("Version    {}  (up to date)", s.current_version)),
    }
    if let (Some(url), true) = (&s.release_notes_url, s.update_available) {
        o.line(format!("Notes      {url}"));
    }
    o.line(format!(
        "Updates    {}",
        match s.mode.as_str() {
            "auto" => "automatic, in the maintenance window",
            "off" => "not checked automatically",
            _ => "notify only",
        }
    ));
    if let Some(e) = &s.check_error {
        o.line(format!("Last check failed: {e}"));
    }
    if let Some(r) = &s.last_run {
        o.line(format!(
            "Last run   {} → {}: {}",
            r.from_version,
            r.to_version,
            o.status(&r.status.replace('_', " "))
        ));
    }
    if s.servers.len() > 1 {
        let mut t = Table::new(&["server", "version", "status"]).status_column(2);
        for sv in &s.servers {
            t.row([
                sv.name.clone(),
                sv.version.clone().unwrap_or_else(|| "-".into()),
                sv.status.clone(),
            ]);
        }
        o.line("");
        o.table(&t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(current: &str, latest: Option<&str>, available: bool) -> UpdateStatus {
        UpdateStatus {
            current_version: current.into(),
            latest_version: latest.map(Into::into),
            update_available: available,
            release_notes_url: None,
            checked_at: None,
            check_error: None,
            mode: "notify".into(),
            can_upgrade: true,
            last_run: None,
            servers: vec![],
        }
    }

    fn run(status: &str) -> Run {
        Run {
            id: "r1".into(),
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            trigger: "manual".into(),
            status: status.into(),
            log: None,
            requested_at: "2026-10-25T00:00:00Z".into(),
            finished_at: None,
        }
    }

    #[test]
    fn what_server_update_does() {
        assert_eq!(next(&status("0.1.0", Some("0.1.0"), false)), Next::UpToDate);
        assert_eq!(next(&status("0.1.0", None, false)), Next::UpToDate);
        assert_eq!(
            next(&status("0.1.0", Some("0.2.0"), true)),
            Next::Upgrade("0.2.0".into())
        );
        let mut s = status("0.1.0", Some("0.2.0"), true);
        s.last_run = Some(run("running"));
        assert_eq!(next(&s), Next::Follow);
        s.last_run = Some(run("failed"));
        assert_eq!(next(&s), Next::Upgrade("0.2.0".into()));
        let mut s = status("0.2.0", Some("0.2.0"), false);
        s.servers.push(ServerVersion {
            node_id: "n1".into(),
            name: "worker-1".into(),
            version: Some("0.1.0".into()),
            status: "failed".into(),
            error: Some("boom".into()),
        });
        assert_eq!(next(&s), Next::RetryServers);
    }

    #[test]
    fn parses_the_servers_status() {
        let body = r#"{
            "currentVersion": "0.1.3", "latestVersion": "0.1.4", "updateAvailable": true,
            "releaseNotesUrl": "https://downloads.usemodula.org/releases/download/v0.1.4/",
            "checkedAt": "2026-10-25T10:00:00Z", "checkError": null, "mode": "notify",
            "channel": "stable", "window": {"days": [0], "time": "03:00", "timezone": "UTC"},
            "nextWindow": null, "canUpgrade": true,
            "lastRun": {"id": "abc", "fromVersion": "0.1.2", "toVersion": "0.1.3",
                        "trigger": "manual", "status": "succeeded", "log": "ok",
                        "requestedAt": "2026-10-24T10:00:00Z", "finishedAt": "2026-10-24T10:02:00Z"},
            "servers": [{"nodeId": "n1", "name": "main", "version": "0.1.3",
                         "status": "current", "error": null}]
        }"#;
        let s: UpdateStatus = serde_json::from_str(body).unwrap();
        assert_eq!(next(&s), Next::Upgrade("0.1.4".into()));
        assert!(!s.rolling_out());
    }
}
