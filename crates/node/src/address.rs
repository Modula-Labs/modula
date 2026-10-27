//! Changing which control plane address this node dials (`node.json`'s
//! `server`), on the control plane's request (`ServerAddress`): when the main
//! server moved, and when private mode routes nodes through Cloudflare Access
//! (crate::access). The new address must work with this node's own identity
//! and pinned root before anything changes, so a wrong address never strands
//! the node.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::bail;

use crate::identity::Identity;

/// How long a new address may take to answer.
pub const PROBE_FOR: Duration = Duration::from_secs(90);

/// Connects (mTLS, pinned root) to `server` until it works or `within` passed.
pub async fn probe(server: &str, id: &Identity, within: Duration) -> anyhow::Result<()> {
    let pin = id.fingerprint()?;
    let started = std::time::Instant::now();
    loop {
        match crate::join::connect(server, pin, Some(id)).await {
            Ok(_) => return Ok(()),
            Err(e) if started.elapsed() > within => {
                return Err(e.context(format!("cannot reach the control plane at {server}")));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

/// Only `https://host[:port]` (no path, no credentials).
pub fn valid_server(server: &str) -> bool {
    let Some(rest) = server.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0))
}

/// Make `dir`'s node dial `server` from now on. Returns whether it changed.
pub async fn switch(dir: &Path, server: &str, within: Duration) -> anyhow::Result<bool> {
    if !valid_server(server) {
        bail!("{server:?} is not an https:// address");
    }
    let mut id = Identity::load(dir)?;
    if id.meta.server == server {
        return Ok(false);
    }
    probe(server, &id, within).await?;
    id.meta.server = server.to_owned();
    id.save()?;
    tracing::info!(%server, "control plane address changed");
    Ok(true)
}

static RESTART: OnceLock<Arc<dyn Fn() + Send + Sync>> = OnceLock::new();

/// How `modula-node run` restarts itself (exit; systemd starts it again with
/// its current unit). Without it a restart request just reconnects.
pub fn register_restart(f: Arc<dyn Fn() + Send + Sync>) {
    let _ = RESTART.set(f);
}

pub fn restarter() -> Option<Arc<dyn Fn() + Send + Sync>> {
    RESTART.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::valid_server;

    #[test]
    fn addresses() {
        for ok in [
            "https://deploy.example.com:7101",
            "https://203.0.113.5:7101",
            "https://127.0.0.1:7101/",
        ] {
            assert!(valid_server(ok), "{ok}");
        }
        for bad in [
            "http://203.0.113.5:7101",
            "https://a@b:7101",
            "https://host:7101/x",
            "https://host:7101?x=1",
            "nonsense",
        ] {
            assert!(!valid_server(bad), "{bad}");
        }
    }
}
