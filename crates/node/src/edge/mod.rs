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
pub mod tcp;

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
    /// More names for this node's own dashboard route, if it serves one.
    pub dashboard_hosts: Vec<String>,
}

#[async_trait]
pub trait EdgeSink: Send + Sync + 'static {
    async fn apply(&self, config: &EdgeConfig) -> anyhow::Result<()>;
    /// Hostnames of this node's own routes (`--route`) to stop serving, e.g.
    /// the dashboard in private mode. Applied from the next `apply`.
    fn set_hidden(&self, _hosts: Vec<String>) {}
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

/// Parses `--route host=127.0.0.1:7100` (a route this node serves itself).
pub fn parse_local_route(s: &str) -> Result<ResolvedRoute, String> {
    let (host, addr) = s.split_once('=').ok_or("expected HOST=ADDR:PORT")?;
    let hostname = normalize_host(host).ok_or_else(|| format!("{host:?} is not a DNS name"))?;
    let upstream: SocketAddr = addr
        .parse()
        .map_err(|_| format!("{addr:?} is not an address like 127.0.0.1:7100"))?;
    Ok(ResolvedRoute { hostname, upstream })
}

/// Lowercase, no trailing dot; `None` for anything that is not a plain DNS
/// name or an IPv4 address (IPv6 literals, ports, empty labels, odd
/// characters). An IPv4 address is a server without a domain: its dashboard
/// is served at `https://<ip>`.
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(ip) = ipv4_host(&h) {
        return Some(ip.to_string());
    }
    // Dotted numbers that aren't a canonical IPv4 (`01.2.3.4`) are neither.
    let numeric = h.split('.').all(|l| l.bytes().all(|b| b.is_ascii_digit()));
    if h.is_empty() || h.len() > 253 || numeric || h.parse::<std::net::IpAddr>().is_ok() {
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

/// `host` as an IPv4 address in its canonical form (no leading zeros).
pub fn ipv4_host(host: &str) -> Option<std::net::Ipv4Addr> {
    host.parse::<std::net::Ipv4Addr>()
        .ok()
        .filter(|ip| ip.to_string() == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_routes() {
        let ip = parse_local_route("203.0.113.10=127.0.0.1:7100").unwrap();
        assert_eq!(ip.hostname, "203.0.113.10");
        let r = parse_local_route("Deploy.Example.com=127.0.0.1:7100").unwrap();
        assert_eq!(r.hostname, "deploy.example.com");
        assert_eq!(r.upstream, "127.0.0.1:7100".parse().unwrap());
        for bad in ["deploy.example.com", "a.com=localhost:1", "a.com=127.0.0.1"] {
            assert!(parse_local_route(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hosts() {
        assert_eq!(
            normalize_host("API.Example.com.").as_deref(),
            Some("api.example.com")
        );
        assert_eq!(normalize_host("1.2.3.4").as_deref(), Some("1.2.3.4"));
        for bad in [
            "",
            "01.2.3.4",
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
