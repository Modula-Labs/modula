//! The node's edge: public hostnames → containers, served by Caddy.
//!
//! The reconciler resolves routes to container addresses and hands them to an
//! [`EdgeSink`]. The Caddy sink renders a complete config, loads it through the
//! admin API (atomic, rolled back by Caddy on error) and keeps the `ask`
//! endpoint's allowlist in sync, so certificates are only ever issued for
//! hostnames this node actually routes.

pub mod ask;
pub mod caddy;
mod http;

use std::net::SocketAddr;

use async_trait::async_trait;
use modula_proto::node::v1::TlsMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub hostname: String,
    pub upstream: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeConfig {
    pub routes: Vec<ResolvedRoute>,
    pub tls: TlsMode,
    pub acme_email: String,
}

#[async_trait]
pub trait EdgeSink: Send + Sync + 'static {
    async fn apply(&self, config: &EdgeConfig) -> anyhow::Result<()>;
}

/// Records the last config instead of serving it (tests).
#[derive(Default)]
pub struct RecordingEdge {
    pub last: std::sync::Mutex<Option<EdgeConfig>>,
}

#[async_trait]
impl EdgeSink for RecordingEdge {
    async fn apply(&self, config: &EdgeConfig) -> anyhow::Result<()> {
        *self.last.lock().expect("edge lock") = Some(config.clone());
        Ok(())
    }
}

/// Lowercase, no trailing dot; `None` for anything that is not a plain DNS
/// name (IP literals, ports, empty labels, odd characters).
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() || h.len() > 253 || h.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let ok = h.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    ok.then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(
            normalize_host("API.Example.com.").as_deref(),
            Some("api.example.com")
        );
        for bad in [
            "",
            "1.2.3.4",
            "::1",
            "a..b",
            "-a.com",
            "a.com:443",
            "a_b.com",
            "a b.com",
        ] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
    }
}
