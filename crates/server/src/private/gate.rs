//! In private mode, refuse every request that didn't come through Cloudflare
//! Access, so the origin can't be reached around it.
//!
//! Allowed without an Access token:
//! - break glass: a direct connection from this machine (an SSH tunnel to
//!   127.0.0.1). The peer address alone is not enough, because cloudflared
//!   and Caddy also connect from loopback; they always add forwarding
//!   headers (`Cf-Connecting-Ip`/`Cf-Ray`, `X-Forwarded-For`), which a
//!   break-glass request has none of;
//! - [`super::bypass_paths`], which Access also lets through (they
//!   authenticate the caller themselves).
//!
//! Everything else needs a `Cf-Access-Jwt-Assertion` signed by the team's
//! Access keys for the dashboard application's audience.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

pub const ASSERTION_HEADER: &str = "cf-access-jwt-assertion";

/// Headers a proxy in front of the server adds; a break-glass request has none.
const PROXY_HEADERS: &[&str] = &[
    "cf-connecting-ip",
    "cf-ray",
    "cf-access-jwt-assertion",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
    "forwarded",
];

pub fn is_break_glass(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    peer.is_some_and(|p| p.ip().is_loopback())
        && !PROXY_HEADERS.iter().any(|h| headers.contains_key(*h))
}

fn is_bypass(path: &str) -> bool {
    super::bypass_paths().contains(&path)
}

fn refuse(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({ "error": { "code": "access_required", "message": message } })),
    )
        .into_response()
}

pub async fn require_access(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(verifier) = state.private.verifier() else {
        return next.run(req).await;
    };
    // As axum's ConnectInfo extractor does (MockConnectInfo in tests).
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .or_else(|| {
            req.extensions()
                .get::<axum::extract::connect_info::MockConnectInfo<SocketAddr>>()
                .map(|c| c.0)
        });
    if is_break_glass(peer, req.headers()) || is_bypass(req.uri().path()) {
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get(ASSERTION_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match verifier.verify(token).await {
        Ok(_) => next.run(req).await,
        Err(e) => {
            tracing::debug!(path = %req.uri().path(), error = %e, "refused a request without a valid Access token");
            refuse("This dashboard is private: sign in through Cloudflare Access.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn break_glass_needs_loopback_and_no_proxy() {
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let none = HeaderMap::new();
        assert!(is_break_glass(Some(local), &none));
        assert!(is_break_glass(Some("[::1]:5000".parse().unwrap()), &none));
        assert!(!is_break_glass(Some(remote), &none));
        assert!(!is_break_glass(None, &none));
        for h in ["cf-ray", "cf-connecting-ip", "x-forwarded-for"] {
            let mut hm = HeaderMap::new();
            hm.insert(h, "1".parse().unwrap());
            assert!(!is_break_glass(Some(local), &hm), "{h}");
        }
    }

    #[test]
    fn bypass_is_exact() {
        assert!(is_bypass("/api/install.sh"));
        assert!(is_bypass("/api/github/webhook"));
        assert!(!is_bypass("/api/install.sh/x"));
        assert!(!is_bypass("/api/auth/me"));
    }
}
