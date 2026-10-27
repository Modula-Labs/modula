//! Turning private mode on and off. Each Cloudflare object is recorded in the
//! settings row as soon as it exists, so [`teardown`] (rollback, disable, or
//! recovery after a restart) finds everything and is safe to run repeatedly.

use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::nodes::{self, NodeRow};
use super::{Settings, aad, settings};
use crate::error::{AppError, AppResult};
use crate::integrations::cloudflare::client::{Client, MANAGED_COMMENT};
use crate::integrations::cloudflare::zero_trust::{NewApp, Policy};
use crate::integrations::cloudflare::{self as cf};
use crate::state::AppState;

/// Everything checked before anything is created.
#[derive(Debug, Clone)]
pub struct Plan {
    pub workspace: Id,
    pub account_id: String,
    pub zone_id: String,
    pub dashboard_host: String,
    pub node_host: String,
    pub team_domain: String,
    pub emails: Vec<String>,
    pub domains: Vec<String>,
    pub main_node: Id,
    pub workers: Vec<Id>,
}

/// One of the dashboard's DNS records before the switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
    pub proxied: Option<bool>,
    pub comment: Option<String>,
}

/// `<first label>-nodes.<rest>` (`modula-nodes.<zone>` for a dashboard at the
/// zone apex): a sibling of the dashboard rather than `nodes.<dashboard>`, so
/// a dashboard one level below the zone keeps the node host there too, where
/// Cloudflare's free Universal SSL certificate covers it (`*.zone` only).
pub fn default_node_host(dashboard: &str, zone: &str) -> String {
    match dashboard.split_once('.') {
        Some((first, rest)) if dashboard != zone => format!("{first}-nodes.{rest}"),
        _ => format!("modula-nodes.{zone}"),
    }
}

fn clean_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    crate::config::normalize_apps_domain(&h)
        .ok()
        .filter(|h| h.contains('.'))
}

fn clean_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_ascii_lowercase();
    let (user, domain) = e.split_once('@')?;
    (!user.is_empty() && domain.contains('.') && !e.contains(char::is_whitespace) && e.len() <= 254)
        .then_some(e)
}

fn tunnel_target(tunnel_id: &str) -> String {
    format!("{tunnel_id}.cfargotunnel.com")
}

fn forbidden_hint(what: &str, permission: &str) -> AppError {
    AppError::bad_request(format!(
        "The Cloudflare token can't manage {what}. Create a new token with {permission} \
         (the template link in Settings → Integrations → Cloudflare has it) and replace the token."
    ))
}

/// Checks everything that can be checked up front. Creates nothing.
pub async fn preflight(
    state: &AppState,
    workspace: Id,
    node_host: Option<&str>,
    emails: &[String],
    domains: &[String],
) -> AppResult<Plan> {
    let s = settings(&state.db).await?;
    if s.phase != "off" {
        return Err(AppError::Conflict(format!(
            "private mode is {} already",
            s.phase
        )));
    }
    if state.cfg.is_local_dev() {
        return Err(AppError::bad_request(
            "private mode needs a public https dashboard hostname, not localhost",
        ));
    }
    let dashboard_host = state
        .cfg
        .public_url
        .host_str()
        .and_then(clean_host)
        .ok_or_else(|| AppError::bad_request("the dashboard URL has no DNS hostname"))?;
    let node_host = match node_host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => Some(
            clean_host(h)
                .ok_or_else(|| AppError::bad_request("the node hostname is not a DNS name"))?,
        ),
        None => None,
    };
    if node_host.as_deref() == Some(dashboard_host.as_str()) {
        return Err(AppError::bad_request(
            "the node hostname must differ from the dashboard's",
        ));
    }
    let emails: Vec<String> = emails
        .iter()
        .map(|e| {
            clean_email(e).ok_or_else(|| AppError::bad_request(format!("{e:?} is not an email")))
        })
        .collect::<AppResult<_>>()?;
    let domains: Vec<String> = domains
        .iter()
        .map(|d| {
            clean_host(d.trim_start_matches('@'))
                .ok_or_else(|| AppError::bad_request(format!("{d:?} is not an email domain")))
        })
        .collect::<AppResult<_>>()?;
    if emails.is_empty() && domains.is_empty() {
        return Err(AppError::bad_request(
            "allow at least one email address or email domain, or nobody can sign in",
        ));
    }

    let conn = cf::connection(state, workspace)
        .await?
        .ok_or_else(|| AppError::Conflict("connect Cloudflare to this workspace first".into()))?;
    let account_id = conn.metadata.account_id.clone().ok_or_else(|| {
        AppError::Conflict(
            "the Cloudflare token can't see an account; reconnect with an account-scoped token"
                .into(),
        )
    })?;
    let c = &conn.client;
    if !c.tunnel_access(&account_id).await.map_err(cf::upstream)? {
        return Err(forbidden_hint(
            "tunnels",
            "Account · Cloudflare Tunnel · Edit",
        ));
    }
    if !c
        .access_apps_access(&account_id)
        .await
        .map_err(cf::upstream)?
    {
        return Err(forbidden_hint(
            "Access applications",
            "Account · Access: Apps and Policies · Edit",
        ));
    }
    let team_domain = c
        .team_domain(&account_id)
        .await
        .map_err(cf::upstream)?
        .ok_or_else(|| {
            AppError::Conflict(
                "Cloudflare Zero Trust isn't set up for this account yet: open one.dash.cloudflare.com once \
                 and pick a team name (the free plan is enough)"
                    .into(),
            )
        })?;
    let zone = c
        .find_zone(&dashboard_host)
        .await
        .map_err(cf::upstream)?
        .filter(|z| z.status == "active")
        .ok_or_else(|| {
            AppError::Conflict(format!(
                "{dashboard_host} is not in an active zone the Cloudflare token can manage"
            ))
        })?;
    let node_host = node_host.unwrap_or_else(|| default_node_host(&dashboard_host, &zone.name));
    let node_zone = c.find_zone(&node_host).await.map_err(cf::upstream)?;
    if node_zone.as_ref().map(|z| &z.id) != Some(&zone.id) {
        return Err(AppError::bad_request(format!(
            "the node hostname must be in the same zone as the dashboard ({})",
            zone.name
        )));
    }

    let all = nodes::list(&state.db).await?;
    let mains: Vec<&NodeRow> = all.iter().filter(|n| n.dashboard).collect();
    let [main] = mains[..] else {
        return Err(AppError::Conflict(
            "can't tell which server runs the dashboard (no server, or several, report serving it); \
             upgrade modula-node on the main server"
                .into(),
        ));
    };
    if main.status != "online" || !main.can_access {
        return Err(AppError::Conflict(format!(
            "the main server {} must be online and up to date",
            main.name
        )));
    }
    let not_ready: Vec<&str> = all
        .iter()
        .filter(|n| n.id != main.id && (n.status != "online" || !n.can_access))
        .map(|n| n.name.as_str())
        .collect();
    if !not_ready.is_empty() {
        return Err(AppError::Conflict(format!(
            "every server must be online and up to date to move to the tunnel; not ready: {}",
            not_ready.join(", ")
        )));
    }
    Ok(Plan {
        workspace,
        account_id,
        zone_id: zone.id,
        dashboard_host,
        node_host,
        team_domain,
        emails,
        domains,
        main_node: main.id,
        workers: all
            .iter()
            .filter(|n| n.id != main.id)
            .map(|n| n.id)
            .collect(),
    })
}

/// The dashboard's progress list (`steps`).
struct Progress {
    steps: Vec<Value>,
}

impl Progress {
    async fn save(&self, state: &AppState) -> anyhow::Result<()> {
        sqlx::query("UPDATE private_access SET steps = $1, updated_at = now()")
            .bind(Value::Array(self.steps.clone()))
            .execute(&state.db)
            .await?;
        Ok(())
    }

    async fn begin(&mut self, state: &AppState, label: &str) -> anyhow::Result<()> {
        self.steps
            .push(json!({ "label": label, "status": "running" }));
        self.save(state).await
    }

    async fn end(&mut self, state: &AppState, status: &str, detail: Option<String>) {
        if let Some(last) = self.steps.last_mut() {
            last["status"] = json!(status);
            if let Some(d) = detail {
                last["detail"] = json!(d);
            }
        }
        let _ = self.save(state).await;
    }
}

async fn client_for(state: &AppState, workspace: Option<Id>) -> anyhow::Result<Client> {
    let ws = workspace.context("no workspace recorded")?;
    let conn = cf::connection(state, ws)
        .await
        .map_err(|e| anyhow!("{e}"))?
        .context("the workspace's Cloudflare connection is gone; reconnect it")?;
    Ok(conn.client)
}

async fn seal(
    state: &AppState,
    ws: Id,
    column: &'static str,
    plain: &str,
) -> anyhow::Result<Vec<u8>> {
    state
        .keys
        .seal(&state.db, plain.as_bytes(), aad(ws, column))
        .await
        .map_err(|e| anyhow!("{e}"))
}

async fn open(
    state: &AppState,
    ws: Id,
    column: &'static str,
    sealed: &[u8],
) -> anyhow::Result<String> {
    let plain = state
        .keys
        .open(&state.db, sealed, aad(ws, column))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(String::from_utf8(plain.to_vec())?)
}

/// Waits until `check` says yes, up to the hooks' `wait`.
async fn wait_for<F, Fut>(state: &AppState, what: &str, mut check: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<bool>>,
{
    let hooks = state.private.hooks();
    let started = Instant::now();
    let mut last_err = None;
    loop {
        match check().await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => last_err = Some(format!("{e:#}")),
        }
        if started.elapsed() >= hooks.wait {
            bail!(
                "{what} (waited {}s{})",
                hooks.wait.as_secs(),
                last_err
                    .map(|e| format!("; last error: {e}"))
                    .unwrap_or_default()
            );
        }
        tokio::time::sleep(hooks.poll).await;
    }
}

fn main_config(
    state: &AppState,
    tunnel_token: String,
    hidden: Vec<String>,
    close: bool,
) -> pb::AccessConfig {
    pb::AccessConfig {
        main: true,
        tunnel_token,
        hidden_routes: hidden,
        close_node_port: close,
        node_port: u32::from(state.cfg.node_listen.port()),
        ..Default::default()
    }
}

fn worker_config(
    state: &AppState,
    node_host: &str,
    client_id: &str,
    secret: &str,
) -> pb::AccessConfig {
    pb::AccessConfig {
        main: false,
        node_host: node_host.to_owned(),
        client_id: client_id.to_owned(),
        client_secret: secret.to_owned(),
        direct_server: state.cfg.node_url.as_str().trim_end_matches('/').to_owned(),
        tunneled_server: tunneled_server(state),
        ..Default::default()
    }
}

/// What servers dial in private mode: their local `cloudflared access tcp`.
pub fn tunneled_server(state: &AppState) -> String {
    format!("https://127.0.0.1:{}", state.cfg.node_listen.port())
}

/// Turns private mode on. On failure everything done is undone and the
/// error is recorded (and returned).
/// Records the plan and enters phase `enabling` (done before answering the
/// request, so the dashboard starts following the progress right away).
pub async fn mark_enabling(state: &AppState, plan: &Plan, by: Option<Id>) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE private_access SET phase = 'enabling', workspace_id = $1, account_id = $2, zone_id = $3,
                dashboard_host = $4, node_host = $5, team_domain = $6, allowed_emails = $7,
                allowed_domains = $8, main_node_id = $9, error = NULL, steps = '[]'::jsonb,
                updated_by = $10, updated_at = now()",
    )
    .bind(plan.workspace)
    .bind(&plan.account_id)
    .bind(&plan.zone_id)
    .bind(&plan.dashboard_host)
    .bind(&plan.node_host)
    .bind(&plan.team_domain)
    .bind(&plan.emails)
    .bind(&plan.domains)
    .bind(plan.main_node)
    .bind(by)
    .execute(&state.db)
    .await?;
    Ok(())
}

pub async fn enable(state: &AppState, plan: Plan, by: Option<Id>) -> anyhow::Result<()> {
    mark_enabling(state, &plan, by).await?;
    let mut progress = Progress { steps: vec![] };
    match run_enable(state, &plan, &mut progress).await {
        Ok(()) => {
            tracing::info!(host = %plan.dashboard_host, "private dashboard is on");
            Ok(())
        }
        Err(e) => {
            let reason = format!("{e:#}");
            progress.end(state, "failed", Some(reason.clone())).await;
            tracing::warn!(error = %reason, "turning on the private dashboard failed; undoing it");
            let warnings = teardown(state, &reason)
                .await
                .unwrap_or_else(|e| vec![format!("{e:#}")]);
            let mut msg = reason;
            if !warnings.is_empty() {
                msg = format!("{msg}. Undoing it also reported: {}", warnings.join("; "));
            }
            sqlx::query("UPDATE private_access SET error = $1")
                .bind(&msg)
                .execute(&state.db)
                .await?;
            Err(anyhow!(msg))
        }
    }
}

async fn run_enable(state: &AppState, plan: &Plan, p: &mut Progress) -> anyhow::Result<()> {
    let client = client_for(state, Some(plan.workspace)).await?;
    let acc = plan.account_id.as_str();
    let ws = plan.workspace;
    let s = settings(&state.db).await?;
    let label = &plan.dashboard_host;

    p.begin(state, "Creating a service token for your servers")
        .await?;
    let (token_id, client_id, secret) = match (
        &s.node_token_id,
        &s.node_token_client_id,
        &s.node_token_secret_sealed,
    ) {
        (Some(id), Some(cid), Some(sealed)) => (
            id.clone(),
            cid.clone(),
            open(state, ws, "node_token_secret_sealed", sealed).await?,
        ),
        _ => {
            let t = client
                .create_service_token(acc, &format!("Modula servers ({label})"))
                .await?;
            let sealed = seal(state, ws, "node_token_secret_sealed", &t.client_secret).await?;
            sqlx::query(
                "UPDATE private_access SET node_token_id = $1, node_token_client_id = $2, node_token_secret_sealed = $3",
            )
            .bind(&t.id)
            .bind(&t.client_id)
            .bind(&sealed)
            .execute(&state.db)
            .await?;
            (t.id, t.client_id, t.client_secret)
        }
    };
    p.end(state, "done", None).await;

    p.begin(state, "Creating the tunnel").await?;
    let tunnel_id = match &s.tunnel_id {
        Some(id) => id.clone(),
        None => {
            let t = client
                .create_tunnel(acc, &format!("modula-{label}"))
                .await?;
            sqlx::query("UPDATE private_access SET tunnel_id = $1")
                .bind(&t.id)
                .execute(&state.db)
                .await?;
            t.id
        }
    };
    client
        .configure_tunnel(
            acc,
            &tunnel_id,
            &[
                (
                    plan.dashboard_host.clone(),
                    format!("http://127.0.0.1:{}", state.cfg.listen.port()),
                ),
                (
                    plan.node_host.clone(),
                    format!("tcp://127.0.0.1:{}", state.cfg.node_listen.port()),
                ),
            ],
        )
        .await?;
    let tunnel_token = client.tunnel_token(acc, &tunnel_id).await?;
    let sealed = seal(state, ws, "tunnel_token_sealed", &tunnel_token).await?;
    sqlx::query("UPDATE private_access SET tunnel_token_sealed = $1")
        .bind(&sealed)
        .execute(&state.db)
        .await?;
    p.end(state, "done", None).await;

    p.begin(state, "Creating the Cloudflare Access applications")
        .await?;
    if s.dashboard_app_id.is_none() {
        let app = client
            .create_access_app(
                acc,
                &NewApp {
                    name: format!("Modula dashboard ({label})"),
                    destinations: vec![plan.dashboard_host.clone()],
                    policies: vec![
                        Policy::people(&plan.emails, &plan.domains, 1),
                        Policy::any_service_token(2),
                    ],
                },
            )
            .await?;
        sqlx::query("UPDATE private_access SET dashboard_app_id = $1, dashboard_aud = $2")
            .bind(&app.id)
            .bind(&app.aud)
            .execute(&state.db)
            .await?;
    }
    if s.bypass_app_id.is_none() {
        let app = client
            .create_access_app(
                acc,
                &NewApp {
                    name: format!("Modula public endpoints ({label})"),
                    destinations: super::bypass_paths()
                        .iter()
                        .map(|p| format!("{}{p}", plan.dashboard_host))
                        .collect(),
                    policies: vec![Policy::bypass(1)],
                },
            )
            .await?;
        sqlx::query("UPDATE private_access SET bypass_app_id = $1")
            .bind(&app.id)
            .execute(&state.db)
            .await?;
    }
    if s.node_app_id.is_none() {
        let app = client
            .create_access_app(
                acc,
                &NewApp {
                    name: format!("Modula servers ({label})"),
                    destinations: vec![plan.node_host.clone()],
                    policies: vec![Policy::service_token(&token_id, 1)],
                },
            )
            .await?;
        sqlx::query("UPDATE private_access SET node_app_id = $1")
            .bind(&app.id)
            .execute(&state.db)
            .await?;
    }
    p.end(state, "done", None).await;

    p.begin(state, "Pointing the node hostname at the tunnel")
        .await?;
    client
        .route_to_tunnel(&plan.zone_id, &plan.node_host, &tunnel_id)
        .await?;
    sqlx::query("UPDATE private_access SET node_dns_created = true")
        .execute(&state.db)
        .await?;
    p.end(state, "done", None).await;

    let agent = state.private.hooks().agent;
    // From here on the servers have something to undo.
    sqlx::query("UPDATE private_access SET nodes_configured = true")
        .execute(&state.db)
        .await?;
    p.begin(state, "Starting cloudflared on the main server")
        .await?;
    agent
        .apply(
            state,
            plan.main_node,
            main_config(state, tunnel_token.clone(), vec![], false),
        )
        .await
        .map_err(|e| anyhow!("the main server could not start cloudflared: {e}"))?;
    p.end(state, "done", None).await;

    p.begin(state, "Waiting for the tunnel to connect").await?;
    wait_for(state, "the tunnel did not report healthy", || async {
        Ok(client.tunnel(acc, &tunnel_id).await?.is_healthy())
    })
    .await?;
    p.end(state, "done", None).await;

    if !plan.workers.is_empty() {
        p.begin(state, "Moving the other servers to the tunnel")
            .await?;
        for node in &plan.workers {
            agent
                .apply(
                    state,
                    *node,
                    worker_config(state, &plan.node_host, &client_id, &secret),
                )
                .await
                .map_err(|e| anyhow!("server {node} could not move to the tunnel: {e}"))?;
        }
        p.end(state, "done", None).await;
    }

    p.begin(state, "Pointing the dashboard at the tunnel")
        .await?;
    switch_dashboard_dns(state, &client, plan, &tunnel_id).await?;
    p.end(state, "done", None).await;

    p.begin(
        state,
        "Checking the dashboard answers through Cloudflare Access",
    )
    .await?;
    let probe = state.private.hooks().probe;
    wait_for(
        state,
        "the dashboard did not answer with the Cloudflare Access sign-in (DNS may still be propagating)",
        || probe.behind_access(&plan.dashboard_host),
    )
    .await?;
    p.end(state, "done", None).await;

    p.begin(state, "Refusing requests that bypass Access")
        .await?;
    sqlx::query("UPDATE private_access SET phase = 'on', updated_at = now()")
        .execute(&state.db)
        .await?;
    super::activate(state, &settings(&state.db).await?);
    // Servers still connected directly now reconnect through the tunnel.
    for node in &plan.workers {
        state.hub.disconnect(*node);
    }
    p.end(state, "done", None).await;

    p.begin(state, "Closing the dashboard on the public edge")
        .await?;
    agent
        .apply(
            state,
            plan.main_node,
            main_config(state, tunnel_token, vec![plan.dashboard_host.clone()], true),
        )
        .await
        .map_err(|e| {
            anyhow!("the main server could not stop serving the dashboard publicly: {e}")
        })?;
    state.hub.kick(plan.main_node);
    p.end(state, "done", None).await;
    Ok(())
}

async fn switch_dashboard_dns(
    state: &AppState,
    client: &Client,
    plan: &Plan,
    tunnel_id: &str,
) -> anyhow::Result<()> {
    let target = tunnel_target(tunnel_id);
    let current: Vec<_> = client
        .records_named(&plan.zone_id, &plan.dashboard_host)
        .await?
        .into_iter()
        .filter(|r| matches!(r.kind.as_str(), "A" | "AAAA" | "CNAME"))
        .collect();
    let (ours, theirs): (Vec<_>, Vec<_>) = current
        .into_iter()
        .partition(|r| r.kind == "CNAME" && r.content.eq_ignore_ascii_case(&target));
    let s = settings(&state.db).await?;
    if s.previous_dns.is_none() {
        let saved: Vec<SavedRecord> = theirs
            .iter()
            .map(|r| SavedRecord {
                kind: r.kind.clone(),
                content: r.content.clone(),
                proxied: r.proxied,
                comment: r.comment.clone(),
            })
            .collect();
        sqlx::query("UPDATE private_access SET previous_dns = $1")
            .bind(serde_json::to_value(saved)?)
            .execute(&state.db)
            .await?;
    }
    // From here on there may be something to restore.
    sqlx::query("UPDATE private_access SET dns_switched = true")
        .execute(&state.db)
        .await?;
    for r in &theirs {
        client.delete_record(&plan.zone_id, &r.id).await?;
    }
    if ours.is_empty() {
        client
            .route_to_tunnel(&plan.zone_id, &plan.dashboard_host, tunnel_id)
            .await?;
    }
    Ok(())
}

async fn restore_dashboard_dns(
    state: &AppState,
    client: &Client,
    s: &Settings,
) -> anyhow::Result<()> {
    let (Some(zone), Some(host)) = (&s.zone_id, &s.dashboard_host) else {
        return Ok(());
    };
    let saved: Vec<SavedRecord> = s
        .previous_dns
        .clone()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let existing = client.records_named(zone, host).await?;
    for r in existing
        .iter()
        .filter(|r| r.kind == "CNAME" && r.content.ends_with(".cfargotunnel.com"))
    {
        client.delete_record(zone, &r.id).await?;
    }
    for r in &saved {
        let present = existing
            .iter()
            .any(|e| e.kind == r.kind && e.content.eq_ignore_ascii_case(&r.content));
        if !present {
            client
                .create_record(
                    zone,
                    &r.kind,
                    host,
                    &r.content,
                    r.proxied,
                    r.comment.as_deref(),
                )
                .await?;
        }
    }
    sqlx::query("UPDATE private_access SET dns_switched = false, previous_dns = NULL")
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Undoes whatever the settings row records, best effort: every step is
/// tried, and what failed comes back as warnings. Ends in phase `off`.
pub async fn teardown(state: &AppState, reason: &str) -> anyhow::Result<Vec<String>> {
    let mut warnings = vec![];
    sqlx::query("UPDATE private_access SET phase = 'disabling', updated_at = now()")
        .execute(&state.db)
        .await?;
    let s = settings(&state.db).await?;
    super::activate(state, &s);
    tracing::info!(%reason, "undoing the private dashboard");
    let agent = state.private.hooks().agent;
    let mut progress = Progress {
        steps: s.steps.as_array().cloned().unwrap_or_default(),
    };
    let ws = s.workspace_id;
    let tunnel_token = match (ws, &s.tunnel_token_sealed) {
        (Some(ws), Some(sealed)) => open(state, ws, "tunnel_token_sealed", sealed).await.ok(),
        _ => None,
    };

    // The public edge serves the dashboard again before DNS points back at it.
    if let (true, Some(main), Some(token)) = (s.nodes_configured, s.main_node_id, &tunnel_token) {
        progress
            .begin(state, "Serving the dashboard on the public edge again")
            .await?;
        let r = agent
            .apply(
                state,
                main,
                main_config(state, token.clone(), vec![], false),
            )
            .await;
        state.hub.kick(main);
        match r {
            Ok(()) => progress.end(state, "done", None).await,
            Err(e) => {
                warnings.push(format!("main server: {e}"));
                progress.end(state, "failed", Some(e)).await;
            }
        }
    }

    let client = match client_for(state, ws).await {
        Ok(c) => Some(c),
        Err(e) if s.tunnel_id.is_some() || s.dns_switched || s.dashboard_app_id.is_some() => {
            warnings.push(format!(
                "{e:#}; remove the Modula tunnel, Access apps and DNS records in Cloudflare by hand"
            ));
            None
        }
        Err(_) => None,
    };

    if s.dns_switched
        && let Some(c) = &client
    {
        progress
            .begin(state, "Restoring the dashboard's DNS records")
            .await?;
        match restore_dashboard_dns(state, c, &s).await {
            Ok(()) => progress.end(state, "done", None).await,
            Err(e) => {
                warnings.push(format!("DNS: {e:#}"));
                progress.end(state, "failed", Some(format!("{e:#}"))).await;
            }
        }
    }

    // Other servers dial the node endpoint directly again.
    let workers: Vec<NodeRow> = nodes::list(&state.db)
        .await?
        .into_iter()
        .filter(|n| Some(n.id) != s.main_node_id && n.can_access)
        .collect();
    if s.nodes_configured && !workers.is_empty() {
        progress
            .begin(state, "Moving the other servers off the tunnel")
            .await?;
        let mut failed = vec![];
        for n in &workers {
            let cfg = worker_config(state, "", "", "");
            if let Err(e) = agent.apply(state, n.id, cfg).await {
                failed.push(format!("{} ({e})", n.name));
            }
        }
        if failed.is_empty() {
            progress.end(state, "done", None).await;
        } else {
            let msg = format!(
                "these servers still dial through the tunnel; see the guide (Private dashboard → stranded servers): {}",
                failed.join(", ")
            );
            warnings.push(msg.clone());
            progress.end(state, "failed", Some(msg)).await;
        }
    }

    if let Some(main) = s.main_node_id
        && s.nodes_configured
    {
        progress
            .begin(state, "Stopping cloudflared on the main server")
            .await?;
        match agent
            .apply(
                state,
                main,
                main_config(state, String::new(), vec![], false),
            )
            .await
        {
            Ok(()) => progress.end(state, "done", None).await,
            Err(e) => {
                warnings.push(format!("main server: {e}"));
                progress.end(state, "failed", Some(e)).await;
            }
        }
    }

    if let Some(c) = &client {
        progress
            .begin(state, "Removing the tunnel and Access applications")
            .await?;
        let acc = s.account_id.clone().unwrap_or_default();
        let mut failed = vec![];
        let mut record = |what: &str, r: anyhow::Result<()>| {
            if let Err(e) = r {
                failed.push(format!("{what}: {e:#}"));
                false
            } else {
                true
            }
        };
        if s.node_dns_created
            && let (Some(zone), Some(host)) = (&s.zone_id, &s.node_host)
        {
            let r = async {
                for r in c.records_named(zone, host).await? {
                    if r.kind == "CNAME" && r.comment.as_deref() == Some(MANAGED_COMMENT) {
                        c.delete_record(zone, &r.id).await?;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            if record("node DNS record", r) {
                sqlx::query("UPDATE private_access SET node_dns_created = false")
                    .execute(&state.db)
                    .await?;
            }
        }
        for (col, id) in [
            ("dashboard_app_id", &s.dashboard_app_id),
            ("bypass_app_id", &s.bypass_app_id),
            ("node_app_id", &s.node_app_id),
        ] {
            if let Some(id) = id
                && record(
                    "Access application",
                    c.delete_access_app(&acc, id).await.map_err(Into::into),
                )
            {
                // Column names come from the fixed list above.
                let sql = match col {
                    "dashboard_app_id" => {
                        "UPDATE private_access SET dashboard_app_id = NULL, dashboard_aud = NULL"
                    }
                    "bypass_app_id" => "UPDATE private_access SET bypass_app_id = NULL",
                    _ => "UPDATE private_access SET node_app_id = NULL",
                };
                sqlx::query(sql).execute(&state.db).await?;
            }
        }
        if let Some(id) = &s.tunnel_id
            && record(
                "tunnel",
                c.delete_tunnel(&acc, id).await.map_err(Into::into),
            )
        {
            sqlx::query("UPDATE private_access SET tunnel_id = NULL, tunnel_token_sealed = NULL")
                .execute(&state.db)
                .await?;
        }
        if let Some(id) = &s.node_token_id
            && record(
                "service token",
                c.delete_service_token(&acc, id).await.map_err(Into::into),
            )
        {
            sqlx::query(
                "UPDATE private_access SET node_token_id = NULL, node_token_client_id = NULL, node_token_secret_sealed = NULL",
            )
            .execute(&state.db)
            .await?;
        }
        if failed.is_empty() {
            progress.end(state, "done", None).await;
        } else {
            let msg = failed.join("; ");
            warnings.push(msg.clone());
            progress.end(state, "failed", Some(msg)).await;
        }
    }

    sqlx::query(
        "UPDATE private_access SET phase = 'off', nodes_configured = false, updated_at = now()",
    )
    .execute(&state.db)
    .await?;
    super::activate(state, &settings(&state.db).await?);
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_host_defaults_to_one_level_below_the_zone() {
        assert_eq!(
            default_node_host("deploy.example.com", "example.com"),
            "deploy-nodes.example.com"
        );
        assert_eq!(
            default_node_host("example.com", "example.com"),
            "modula-nodes.example.com"
        );
    }

    #[test]
    fn input_cleaning() {
        assert_eq!(
            clean_email(" Ana@Example.com ").as_deref(),
            Some("ana@example.com")
        );
        assert!(clean_email("nope").is_none());
        assert!(clean_email("a b@x.com").is_none());
        assert_eq!(clean_host("Example.COM.").as_deref(), Some("example.com"));
        assert!(clean_host("localhost").is_none());
        assert!(clean_host("https://x.com").is_none());
    }
}
