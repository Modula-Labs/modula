//! Private dashboard: Cloudflare Tunnel + Cloudflare Access in front of the
//! dashboard, the API, the CLI endpoint and the node endpoint, while apps stay
//! public on their own domains (Caddy keeps serving them on 80/443).
//!
//! Turning it on ([`orchestrate::enable`]), using a workspace's Cloudflare
//! connection (its token needs Cloudflare Tunnel: Edit and Access: Apps and
//! Policies: Edit):
//!
//! 1. a service token for the servers, a remotely configured tunnel
//!    (`dashboard → http://127.0.0.1:<http port>`, `node host → tcp://127.0.0.1:<node port>`),
//!    and three Access applications: the dashboard (allowed emails/domains,
//!    plus any service token for the CLI and CI), a bypass for the endpoints
//!    that authenticate themselves (installer, release files, build sources,
//!    GitHub webhooks), and the node host (the servers' service token only);
//! 2. a proxied CNAME for the node host; the main server's modula-node (root)
//!    installs cloudflared and runs the tunnel; Modula waits until Cloudflare
//!    reports it healthy; the other servers move their node connection to a
//!    local `cloudflared access tcp` listener;
//! 3. the dashboard's DNS records are replaced by a proxied CNAME to the
//!    tunnel (the old ones are kept to restore), and Modula waits until the
//!    dashboard answers with the Access login redirect;
//! 4. only then: every dashboard/API request must carry a valid
//!    `Cf-Access-Jwt-Assertion` ([`gate`]), the node endpoint only accepts
//!    connections from loopback (cloudflared), and the main server's edge
//!    stops serving the dashboard hostname and closes the node port in ufw.
//!
//! A failure at any step undoes what was done ([`orchestrate::teardown`],
//! idempotent, also used to turn it off). Break glass:
//! `ssh -L 7100:127.0.0.1:7100 server`, then http://localhost:7100.

pub mod api;
pub mod gate;
pub mod jwt;
pub mod nodes;
pub mod orchestrate;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use tokio::sync::oneshot;

use crate::error::AppResult;
use crate::state::AppState;

/// The settings row (`private_access`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct Settings {
    pub phase: String,
    pub workspace_id: Option<Id>,
    pub account_id: Option<String>,
    pub zone_id: Option<String>,
    pub dashboard_host: Option<String>,
    pub node_host: Option<String>,
    pub allowed_emails: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub team_domain: Option<String>,
    pub tunnel_id: Option<String>,
    pub tunnel_token_sealed: Option<Vec<u8>>,
    pub dashboard_app_id: Option<String>,
    pub dashboard_aud: Option<String>,
    pub bypass_app_id: Option<String>,
    pub node_app_id: Option<String>,
    pub node_token_id: Option<String>,
    pub node_token_client_id: Option<String>,
    pub node_token_secret_sealed: Option<Vec<u8>>,
    pub main_node_id: Option<Id>,
    pub previous_dns: Option<serde_json::Value>,
    pub dns_switched: bool,
    pub node_dns_created: bool,
    pub nodes_configured: bool,
    pub steps: serde_json::Value,
    pub error: Option<String>,
}

pub async fn settings(db: &sqlx::PgPool) -> AppResult<Settings> {
    Ok(sqlx::query_as(
        "SELECT phase, workspace_id, account_id, zone_id, dashboard_host, node_host,
                allowed_emails, allowed_domains, team_domain, tunnel_id, tunnel_token_sealed,
                dashboard_app_id, dashboard_aud, bypass_app_id, node_app_id, node_token_id,
                node_token_client_id, node_token_secret_sealed, main_node_id, previous_dns,
                dns_switched, node_dns_created, nodes_configured, steps, error
         FROM private_access",
    )
    .fetch_one(db)
    .await?)
}

/// AAD for the secrets in `private_access`, sealed with `workspace`'s key.
pub fn aad(workspace: Id, column: &'static str) -> FieldAad<'static> {
    FieldAad {
        scope: workspace,
        table: "private_access",
        column,
        row: "instance",
    }
}

/// The paths Access lets through without signing in, and the origin accepts
/// without an Access token: each authenticates the caller itself (or serves
/// public files) and is needed by machines that can't sign in.
pub fn bypass_paths() -> [&'static str; 4] {
    [
        // Servers joining in private mode fetch the installer and binaries.
        "/api/install.sh",
        "/api/releases",
        // Servers download uploaded sources with a per-upload token.
        crate::deploy::upload::DOWNLOAD_PATH,
        // GitHub's webhooks (HMAC-signed).
        crate::api::github::WEBHOOK_PATH,
    ]
}

/// Probes whether a hostname is behind Access (the real one asks it over
/// HTTPS; tests answer directly).
pub trait Probe: Send + Sync {
    fn behind_access<'a>(
        &'a self,
        host: &'a str,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<bool>>;
}

/// `GET https://<host>/api/health` without following redirects: Access
/// answers unauthenticated requests with a redirect to `<team>.cloudflareaccess.com`.
pub struct HttpsProbe;

impl Probe for HttpsProbe {
    fn behind_access<'a>(
        &'a self,
        host: &'a str,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let http = reqwest::Client::builder()
                .user_agent(concat!("modula-server/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let res = http
                .get(format!("https://{host}/api/health"))
                .send()
                .await?;
            Ok(is_access_redirect(
                res.status().as_u16(),
                res.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
            ))
        })
    }
}

/// Whether a response is Cloudflare Access asking the caller to sign in.
pub fn is_access_redirect(status: u16, location: Option<&str>) -> bool {
    let Some(loc) = location else { return false };
    let to_access = url::Url::parse(loc).ok().is_some_and(|u| {
        u.host_str()
            .is_some_and(|h| h.ends_with(".cloudflareaccess.com"))
            || u.path().starts_with("/cdn-cgi/access/login")
    });
    (300..400).contains(&status) && to_access
}

/// Runtime state: the active gate, and answers from nodes.
pub struct Runtime {
    verifier: RwLock<Option<Arc<jwt::Verifier>>>,
    /// AccessConfig request id → (node that must answer, the waiting caller).
    pending: Mutex<HashMap<String, (Id, oneshot::Sender<pb::AccessStatus>)>>,
    hooks: RwLock<Hooks>,
    /// One enable/disable at a time.
    pub(crate) busy: tokio::sync::Mutex<()>,
}

/// What tests replace.
#[derive(Clone)]
pub struct Hooks {
    pub keys: Arc<dyn jwt::KeySource>,
    pub probe: Arc<dyn Probe>,
    pub agent: Arc<dyn nodes::Agent>,
    /// How long to wait for the tunnel and for the Access redirect, and how
    /// often to look.
    pub wait: std::time::Duration,
    pub poll: std::time::Duration,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            verifier: RwLock::new(None),
            pending: Mutex::new(HashMap::new()),
            hooks: RwLock::new(Hooks {
                keys: Arc::new(jwt::CloudflareCerts),
                probe: Arc::new(HttpsProbe),
                agent: Arc::new(nodes::HubAgent),
                wait: std::time::Duration::from_secs(180),
                poll: std::time::Duration::from_secs(3),
            }),
            busy: tokio::sync::Mutex::new(()),
        }
    }
}

impl Runtime {
    /// The Access token verifier while private mode is on.
    pub fn verifier(&self) -> Option<Arc<jwt::Verifier>> {
        self.verifier.read().expect("private lock").clone()
    }

    pub fn is_on(&self) -> bool {
        self.verifier().is_some()
    }

    fn set_verifier(&self, v: Option<jwt::Verifier>) {
        *self.verifier.write().expect("private lock") = v.map(Arc::new);
    }

    pub fn hooks(&self) -> Hooks {
        self.hooks.read().expect("private lock").clone()
    }

    /// Replaces the key source, probe, node agent or timings (tests).
    pub fn set_hooks(&self, f: impl FnOnce(&mut Hooks)) {
        f(&mut self.hooks.write().expect("private lock"));
    }

    /// Whether a node connection from `peer` is accepted: in private mode only
    /// through the tunnel (cloudflared on this machine) or this machine itself.
    pub fn node_peer_allowed(&self, peer: Option<std::net::SocketAddr>) -> bool {
        !self.is_on() || peer.is_some_and(|p| p.ip().is_loopback())
    }

    /// Hand a node's AccessStatus to whoever waits for it.
    pub fn deliver(&self, node: Id, s: pb::AccessStatus) {
        let mut pending = self.pending.lock().expect("private lock");
        if pending
            .get(&s.request_id)
            .is_some_and(|(owner, _)| *owner == node)
            && let Some((_, tx)) = pending.remove(&s.request_id)
        {
            let _ = tx.send(s);
        }
    }
}

/// Turns the gate on for a settings row in phase `on` (and off otherwise).
pub fn activate(state: &AppState, s: &Settings) {
    let v = match (s.phase.as_str(), &s.team_domain, &s.dashboard_aud) {
        ("on", Some(team), Some(aud)) => Some(jwt::Verifier::new(
            team,
            aud,
            state.private.hooks().keys.clone(),
        )),
        _ => None,
    };
    state.private.set_verifier(v);
}

/// At startup: restore the gate, and finish an enable or disable that a
/// restart interrupted (by undoing it: the admin starts it again).
pub async fn startup(state: AppState) {
    let s = match settings(&state.db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "cannot read the private dashboard settings");
            return;
        }
    };
    activate(&state, &s);
    if matches!(s.phase.as_str(), "enabling" | "disabling") {
        tracing::warn!(phase = %s.phase, "a private dashboard change was interrupted; undoing it");
        let _guard = state.private.busy.lock().await;
        if let Err(e) = orchestrate::teardown(&state, "interrupted by a restart").await {
            tracing::error!(
                error = format!("{e:#}"),
                "could not undo the private dashboard change"
            );
        }
    }
}
