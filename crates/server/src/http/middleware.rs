use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::state::AppState;

/// Paths that authenticate by signature instead of cookies (no Origin check).
const SIGNED_WEBHOOK_PREFIX: &str = "/api/webhooks/";

/// CSRF defence: every state-changing request must come from our exact origin.
/// Browsers always send `Origin` on cross-origin and same-origin non-GET fetches,
/// so a missing header on a mutation is rejected too.
// TODO(C6): exempt requests authenticated by a Bearer API token (the CLI sends no
// Origin). The exemption must come from token auth, not from the header alone.
pub async fn require_same_origin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if safe || req.uri().path().starts_with(SIGNED_WEBHOOK_PREFIX) {
        return next.run(req).await;
    }
    let origin_ok = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| o == state.cfg.public_origin());
    if !origin_ok {
        tracing::warn!(path = %req.uri().path(), "rejected cross-origin or origin-less mutation");
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

pub async fn security_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: https://avatars.githubusercontent.com; font-src 'self'; \
         connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; \
         frame-ancestors 'none'",
    );
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "strict-origin-when-cross-origin");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(
        h,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=()",
    );
    if state.cfg.secure_cookies() {
        set(
            h,
            "strict-transport-security",
            "max-age=63072000; includeSubDomains",
        );
    }
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}
