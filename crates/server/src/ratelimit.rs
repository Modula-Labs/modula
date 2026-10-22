//! Fixed-window rate limiting. Redis in production; in-memory for tests.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use redis::aio::ConnectionManager;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub max: u32,
    pub window: Duration,
}

impl Limit {
    pub const fn per_minute(max: u32) -> Self {
        Self {
            max,
            window: Duration::from_secs(60),
        }
    }
    pub const fn per(max: u32, window: Duration) -> Self {
        Self { max, window }
    }
}

pub enum RateLimiter {
    Redis(ConnectionManager),
    Memory(Mutex<HashMap<String, (u32, Instant)>>),
}

// INCR, set the expiry on the first hit, return (count, remaining ttl ms).
const SCRIPT: &str = r"
local n = redis.call('INCR', KEYS[1])
if n == 1 then redis.call('PEXPIRE', KEYS[1], ARGV[1]) end
local ttl = redis.call('PTTL', KEYS[1])
if ttl < 0 then redis.call('PEXPIRE', KEYS[1], ARGV[1]); ttl = tonumber(ARGV[1]) end
return {n, ttl}
";

impl RateLimiter {
    pub fn memory() -> Self {
        Self::Memory(Mutex::new(HashMap::new()))
    }

    /// Count a hit for `bucket`/`key`; error with 429 once the window is exhausted.
    /// `key` is hashed-in as-is: callers pass ids/IPs/normalized emails, never secrets.
    pub async fn check(&self, bucket: &str, key: &str, limit: Limit) -> AppResult<()> {
        let k = format!("modula:rl:{bucket}:{key}");
        let (count, ttl_ms) = match self {
            Self::Redis(conn) => {
                let mut conn = conn.clone();
                let (n, ttl): (u32, i64) = redis::Script::new(SCRIPT)
                    .key(&k)
                    .arg(limit.window.as_millis() as u64)
                    .invoke_async(&mut conn)
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                (n, ttl.max(0) as u64)
            }
            Self::Memory(map) => {
                let mut map = map.lock().expect("ratelimit lock");
                let now = Instant::now();
                let entry = map.entry(k).or_insert((0, now + limit.window));
                if entry.1 <= now {
                    *entry = (0, now + limit.window);
                }
                entry.0 += 1;
                (
                    entry.0,
                    entry.1.saturating_duration_since(now).as_millis() as u64,
                )
            }
        };
        if count > limit.max {
            return Err(AppError::TooManyRequests {
                retry_after_secs: ttl_ms.div_ceil(1000).max(1),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_limiter_blocks_after_max() {
        let rl = RateLimiter::memory();
        let lim = Limit::per_minute(2);
        rl.check("login", "1.2.3.4", lim).await.unwrap();
        rl.check("login", "1.2.3.4", lim).await.unwrap();
        let err = rl.check("login", "1.2.3.4", lim).await.unwrap_err();
        assert!(
            matches!(err, AppError::TooManyRequests { retry_after_secs } if retry_after_secs >= 1)
        );
        // Other keys and buckets are independent.
        rl.check("login", "5.6.7.8", lim).await.unwrap();
        rl.check("setup", "1.2.3.4", lim).await.unwrap();
    }

    #[tokio::test]
    async fn memory_limiter_window_resets() {
        let rl = RateLimiter::memory();
        let lim = Limit::per(1, Duration::from_millis(30));
        rl.check("b", "k", lim).await.unwrap();
        assert!(rl.check("b", "k", lim).await.is_err());
        tokio::time::sleep(Duration::from_millis(40)).await;
        rl.check("b", "k", lim).await.unwrap();
    }
}
