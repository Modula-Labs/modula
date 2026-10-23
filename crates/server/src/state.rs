use std::ops::Deref;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::AppResult;
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
    /// Cancelled when the server starts shutting down; long-lived streams
    /// (node sessions, log follows) end on it so shutdown stays fast.
    pub shutdown: CancellationToken,
    pki: tokio::sync::OnceCell<Pki>,
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config, keys: Keyring, limiter: RateLimiter) -> Self {
        Self(Arc::new(Inner {
            db,
            cfg,
            keys,
            limiter,
            jobs_notify: Notify::new(),
            hub: Hub::default(),
            shutdown: CancellationToken::new(),
            pki: tokio::sync::OnceCell::new(),
        }))
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
