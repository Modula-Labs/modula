use std::ops::Deref;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Notify;

use crate::config::Config;
use crate::keyring::Keyring;
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
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config, keys: Keyring, limiter: RateLimiter) -> Self {
        Self(Arc::new(Inner {
            db,
            cfg,
            keys,
            limiter,
            jobs_notify: Notify::new(),
        }))
    }
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
