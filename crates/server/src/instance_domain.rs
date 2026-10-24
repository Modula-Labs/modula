//! The instance's own names, set in the dashboard after install: the
//! dashboard's domain and the parent of generated app domains.
//!
//! A server installs without a domain and is reached at its public IP
//! (`MODULA_PUBLIC_URL=https://<ip>`). That address stays valid forever: it
//! is how nodes reach the server (their certificates name it) and the CLI
//! keeps working with it. A dashboard domain is an extra name for the same
//! dashboard: the node that serves the dashboard routes it too, browsers may
//! use either origin, and links the server builds (setup, GitHub, install
//! commands) prefer it. `MODULA_APPS_DOMAIN` is the fallback for the apps
//! domain until one is set here.

use std::net::Ipv4Addr;

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::config::{Config, normalize_apps_domain};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// The names set in the dashboard (None: not set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hosts {
    pub dashboard: Option<String>,
    pub apps: Option<String>,
}

impl Hosts {
    /// What the server answers to before anything is set here.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            dashboard: None,
            apps: cfg.apps_domain.clone(),
        }
    }
}

impl AppState {
    pub fn hosts(&self) -> Hosts {
        self.hosts_lock().read().expect("hosts lock").clone()
    }

    /// The dashboard's canonical URL: its domain once set, else the address
    /// it was installed with.
    pub fn public_url(&self) -> Url {
        match self.hosts().dashboard {
            Some(d) => format!("https://{d}/").parse().expect("validated hostname"),
            None => self.cfg.public_url.clone(),
        }
    }

    pub fn public_origin(&self) -> String {
        self.public_url().origin().ascii_serialization()
    }

    /// Origins browsers may send for the dashboard: the install address
    /// always (the CLI and anyone mid-switch), plus the domain.
    pub fn is_dashboard_origin(&self, origin: &str) -> bool {
        origin == self.cfg.public_origin() || origin == self.public_origin()
    }

    pub fn apps_domain(&self) -> Option<String> {
        self.hosts().apps
    }
}

/// Reads the names saved in the dashboard (startup).
pub async fn load(state: &AppState) -> anyhow::Result<()> {
    let (dashboard, apps): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT dashboard_domain, apps_domain FROM instance")
            .fetch_one(&state.db)
            .await?;
    let mut h = Hosts::from_config(&state.cfg);
    h.dashboard = dashboard;
    if apps.is_some() {
        h.apps = apps;
    }
    *state.hosts_lock().write().expect("hosts lock") = h;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainSettings {
    /// Where the server was installed (`https://<ip>` without a domain).
    install_url: String,
    /// The IPv4 DNS records should point at, when known.
    server_ip: Option<String>,
    dashboard: Option<String>,
    apps_domain: Option<String>,
    /// The apps domain comes from MODULA_APPS_DOMAIN, not this page.
    apps_domain_from_env: bool,
}

async fn settings(state: &AppState) -> AppResult<DomainSettings> {
    let h = state.hosts();
    let saved: Option<String> = sqlx::query_scalar("SELECT apps_domain FROM instance")
        .fetch_one(&state.db)
        .await?;
    Ok(DomainSettings {
        install_url: state
            .cfg
            .public_url
            .as_str()
            .trim_end_matches('/')
            .to_owned(),
        server_ip: server_ips(state).await.first().map(ToString::to_string),
        dashboard: h.dashboard,
        apps_domain_from_env: saved.is_none() && h.apps.is_some(),
        apps_domain: h.apps,
    })
}

/// The addresses a domain must point at: the install address itself when
/// it is an IP, else what its name resolves to.
async fn server_ips(state: &AppState) -> Vec<Ipv4Addr> {
    match state.cfg.public_url.host() {
        Some(url::Host::Ipv4(ip)) => vec![ip],
        Some(url::Host::Domain(d)) => {
            let dns = crate::domains::resolver(state);
            dns.ipv4(d).await.unwrap_or_default()
        }
        _ => vec![],
    }
}

pub async fn get(State(state): State<AppState>) -> AppResult<Json<DomainSettings>> {
    settings(&state).await.map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Update {
    /// Empty or null removes it.
    dashboard: Option<String>,
    apps_domain: Option<String>,
    /// Save even if DNS doesn't point here yet (e.g. a proxied record).
    #[serde(default)]
    skip_dns_check: bool,
}

/// A hostname like deploy.example.com (no IPs, ports, paths, wildcards).
fn hostname(raw: &str, what: &str) -> AppResult<Option<String>> {
    let t = raw.trim();
    let t = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let t = t.trim_end_matches('/');
    if t.is_empty() {
        return Ok(None);
    }
    let h = normalize_apps_domain(t).map_err(|_| {
        AppError::bad_request(format!("{what} must be a hostname like deploy.example.com"))
    })?;
    if h.split('.').count() < 2
        || h.rsplit('.')
            .next()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(AppError::bad_request(format!(
            "{what} must be a hostname like deploy.example.com"
        )));
    }
    Ok(Some(h))
}

/// Whether `name` resolves to one of `ips` (an error explains what it found).
async fn points_here(state: &AppState, name: &str, ips: &[Ipv4Addr]) -> Result<(), String> {
    if ips.is_empty() {
        return Ok(());
    }
    let dns = crate::domains::resolver(state);
    match dns.ipv4(name).await {
        Ok(found) if found.iter().any(|ip| ips.contains(ip)) => Ok(()),
        Ok(found) if found.is_empty() => Err(format!("{name} doesn't resolve yet")),
        Ok(found) => Err(format!(
            "{name} points at {}, not this server",
            found
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(e) => Err(format!("couldn't look up {name}: {e}")),
    }
}

pub async fn put(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<Update>,
) -> AppResult<Json<DomainSettings>> {
    let dashboard = match req.dashboard.as_deref() {
        Some(d) => hostname(d, "the dashboard domain")?,
        None => None,
    };
    let apps = match req.apps_domain.as_deref() {
        Some(d) => hostname(d, "the apps domain")?,
        None => None,
    };
    if let (Some(d), Some(a)) = (&dashboard, &apps)
        && (d == a || d.ends_with(&format!(".{a}")))
    {
        return Err(AppError::bad_request(
            "the dashboard can't be under the apps domain (those names are generated for services)",
        ));
    }
    if !req.skip_dns_check {
        let ips = server_ips(&state).await;
        let ip_list = ips
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut problems = Vec::new();
        if let Some(d) = &dashboard
            && let Err(e) = points_here(&state, d, &ips).await
        {
            problems.push(format!("{e}: add an A record {d} → {ip_list}"));
        }
        if let Some(a) = &apps {
            // A wildcard answers for any name under it.
            let probe = format!(
                "modula-check-{}.{a}",
                hex::encode(modula_crypto::random_bytes::<4>().map_err(anyhow::Error::from)?)
            );
            if let Err(e) = points_here(&state, &probe, &ips).await {
                problems.push(format!(
                    "{}: add an A record *.{a} → {ip_list}",
                    e.replace(&probe, &format!("*.{a}"))
                ));
            }
        }
        if !problems.is_empty() {
            return Err(AppError::Conflict(problems.join("; ")));
        }
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE instance SET dashboard_domain = $1, apps_domain = $2")
        .bind(&dashboard)
        .bind(&apps)
        .execute(&mut *tx)
        .await?;
    // Every node's edge learns the dashboard's names (only the one serving
    // the dashboard uses them).
    let nodes: Vec<Id> = sqlx::query_scalar("SELECT id FROM nodes WHERE status <> 'revoked'")
        .fetch_all(&mut *tx)
        .await?;
    for n in &nodes {
        crate::deploy::bump(&mut *tx, *n).await?;
    }
    Entry::new("instance.domain.update", &meta)
        .actor(&actor)
        .metadata(json!({
            "dashboard": dashboard,
            "appsDomain": apps,
            "skippedDnsCheck": req.skip_dns_check,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    {
        let mut h = Hosts::from_config(&state.cfg);
        h.dashboard = dashboard;
        if apps.is_some() {
            h.apps = apps;
        }
        *state.hosts_lock().write().expect("hosts lock") = h;
    }
    state.hub.kick_all(&nodes);
    crate::github::app::queue_hook_sync(&state.db).await?;
    settings(&state).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_only() {
        let ok = |s: &str| hostname(s, "x").unwrap();
        assert_eq!(
            ok(" https://Deploy.Example.com/ ").as_deref(),
            Some("deploy.example.com")
        );
        assert_eq!(ok(""), None);
        for bad in [
            "1.2.3.4",
            "localhost",
            "*.example.com",
            "a.com:443",
            "a.com/x",
            "a b.com",
        ] {
            assert!(hostname(bad, "x").is_err(), "{bad}");
        }
    }
}
