//! Per-request client metadata (IP, user agent).

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header;
use axum::http::request::Parts;

use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct RequestMeta {
    pub ip: IpAddr,
    pub user_agent: Option<String>,
}

impl RequestMeta {
    pub fn ip_string(&self) -> String {
        self.ip.to_string()
    }
}

impl FromRequestParts<AppState> for RequestMeta {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        let xff = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        let user_agent = parts
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(512).collect());
        Ok(Self {
            ip: client_ip(peer, xff, &state.cfg.trusted_proxies),
            user_agent,
        })
    }
}

/// `X-Forwarded-For` is honoured only when the direct peer is a trusted proxy.
/// Walk right-to-left and return the first address that is not a trusted proxy,
/// so a client-supplied leftmost entry can never spoof its IP.
pub fn client_ip(peer: Option<IpAddr>, xff: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    let peer = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !trusted.contains(&peer) {
        return peer;
    }
    let Some(xff) = xff else { return peer };
    let mut last = peer;
    for part in xff.rsplit(',') {
        match part.trim().parse::<IpAddr>() {
            Ok(ip) if trusted.contains(&ip) => last = ip,
            Ok(ip) => return ip,
            Err(_) => return last,
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_ignores_xff() {
        let t = [ip("127.0.0.1")];
        assert_eq!(
            client_ip(Some(ip("9.9.9.9")), Some("1.1.1.1"), &t),
            ip("9.9.9.9")
        );
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted() {
        let t = [ip("127.0.0.1")];
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), Some("6.6.6.6, 2.2.2.2"), &t),
            ip("2.2.2.2")
        );
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), Some("2.2.2.2, 127.0.0.1"), &t),
            ip("2.2.2.2")
        );
        assert_eq!(client_ip(Some(ip("127.0.0.1")), None, &t), ip("127.0.0.1"));
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), Some("garbage"), &t),
            ip("127.0.0.1")
        );
    }
}
