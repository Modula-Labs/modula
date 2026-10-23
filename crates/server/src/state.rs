use std::ops::Deref;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::AppResult;
use crate::github;
use crate::keyring::Keyring;
use crate::nodes::Hub;
use crate::pki::Pki;
use crate::ratelimit::RateLimiter;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: PgPool,
    pub cfg: Config,
    pub keys: Keyring,
    pub limiter: RateLimiter,
    /// Wakes the in-process job worker right after an enqueue.
    pub jobs_notify: Notify,
    /// Attached nodes.
    pub hub: Hub,
    /// Open database tunnels (relayed over the node streams).
    pub tunnels: crate::tunnel::Tunnels,
    /// Cancelled when the server starts shutting down; long-lived streams
    /// (node sessions, log follows) end on it so shutdown stays fast.
    pub shutdown: CancellationToken,
    pub github: github::Client,
    /// DNS lookups for custom domain verification (swapped out in tests).
    pub dns: std::sync::RwLock<Arc<dyn crate::domains::dns::Resolver>>,
    /// Source uploads in progress per workspace (see `deploy::upload::Slot`).
    pub uploads_in_flight: std::sync::Mutex<std::collections::HashMap<modula_id::Id, u32>>,
    pki: tokio::sync::OnceCell<Pki>,
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config, keys: Keyring, limiter: RateLimiter) -> Self {
        let github = github::Client::new(cfg.github_api.clone()).expect("HTTP client");
        Self(Arc::new(Inner {
            db,
            cfg,
            keys,
            limiter,
            jobs_notify: Notify::new(),
            hub: Hub::default(),
            tunnels: crate::tunnel::Tunnels::default(),
            shutdown: CancellationToken::new(),
            github,
            dns: std::sync::RwLock::new(Arc::new(crate::domains::dns::PublicDns)),
            uploads_in_flight: Default::default(),
            pki: tokio::sync::OnceCell::new(),
        }))
    }

    /// Replaces the DNS resolver used to verify custom domains (tests).
    pub fn set_dns_resolver(&self, r: Arc<dyn crate::domains::dns::Resolver>) {
        *self.0.dns.write().expect("dns lock") = r;
    }

    /// The internal CA, created on first use.
    pub async fn pki(&self) -> AppResult<&Pki> {
        self.0
            .pki
            .get_or_try_init(|| async { Ok(Pki::load_or_init(&self.db, &self.keys).await?) })
            .await
    }
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
